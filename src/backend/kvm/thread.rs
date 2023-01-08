// SPDX-License-Identifier: Apache-2.0

use super::super::Command;
use super::KeepPersonality;
#[cfg(feature = "gdb")]
use crate::backend::execute_gdb;
use crate::backend::sev::set_memory_attributes;
use crate::backend::sev::snp::ghcb::Ghcb;
use crate::backend::sev::snp::ghcb::SnpPscDesc;

use std::io;
use std::iter;
use std::mem::size_of;
use std::sync::{Arc, RwLock};

use anyhow::ensure;
use anyhow::{bail, Context, Result};
use kvm_ioctls::{VcpuExit, VcpuFd};
use mmarinus::{perms, Map};
use sallyport::item::enarxcall::Payload;
use sallyport::item::{Block, Item};
use sallyport::{item, KVM_SYSCALL_TRIGGER_PORT};
use tracing::warn;

pub struct Thread<P: KeepPersonality> {
    keep: Arc<RwLock<super::Keep<P>>>,
    vcpu_fd: Option<VcpuFd>,

    #[cfg(feature = "gdb")]
    gdb_fd: Option<std::net::TcpStream>,
}

impl<P: KeepPersonality> Drop for Thread<P> {
    fn drop(&mut self) {
        let vcpu_fd = self.vcpu_fd.take().unwrap();
        self.keep.write().unwrap().cpu_fds.push(vcpu_fd);
    }
}

impl<P: KeepPersonality + 'static> super::super::Keep for RwLock<super::Keep<P>> {
    fn spawn(self: Arc<Self>) -> Result<Option<Box<dyn super::super::Thread>>> {
        let cpu_opt = self.write().unwrap().cpu_fds.pop();
        match cpu_opt {
            None => Ok(None),
            Some(vcpu_fd) => Ok(Some(Box::new(Thread {
                keep: self,
                vcpu_fd: Some(vcpu_fd),

                #[cfg(feature = "gdb")]
                gdb_fd: None,
            }))),
        }
    }
}

impl<P: KeepPersonality> Thread<P> {
    pub fn balloon(
        &mut self,
        log2: usize,
        npgs: usize,
        addr: usize,
        is_private: bool,
    ) -> sallyport::Result<usize> {
        let size: usize = 1 << log2; // Page Size

        // Get the current page size
        let pgsz = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) } as usize;
        assert!(pgsz.is_power_of_two());

        // Check that the page size is supported and addr is aligned
        if size != pgsz || addr % size != 0 {
            return Err(libc::EINVAL);
        }

        // Allocate the new memory
        let pages = Map::bytes(size * npgs)
            .anywhere()
            .anonymously()
            .with(perms::ReadWrite)
            .map_err(|e| e.err.raw_os_error().unwrap_or(libc::ENOTSUP))?;

        let mut keep = self.keep.write().unwrap();

        // Map the memory into the VM
        Ok(keep
            .map(pages, addr, is_private)
            .map_err(|e| e.raw_os_error().unwrap_or(libc::ENOTSUP))?
            .0
            .userspace_addr as _)
    }

    pub fn meminfo(&self) -> sallyport::Result<usize> {
        let keep = self.keep.read().unwrap();

        // The maximum number of memory slots possible for a virtual machine
        // minus the ones which were already used.
        Ok(keep.kvm_fd.get_nr_memslots() - keep.regions.len())
    }

    fn kvm_enarxcall<'a>(
        &mut self,
        enarxcall: &'a mut Payload,
        data: &'a mut [u8],
    ) -> Result<Option<Item<'a>>> {
        match enarxcall {
            item::Enarxcall {
                num: item::enarxcall::Number::MemInfo,
                ret,
                ..
            } => {
                *ret = match self.meminfo() {
                    Ok(n) => n,
                    Err(e) => -e as usize,
                };
                Ok(None)
            }

            item::Enarxcall {
                num: item::enarxcall::Number::BalloonMemory,
                argv: [log2, npgs, addr, is_private, ..],
                ret,
            } => {
                *ret = match self.balloon(*log2, *npgs, *addr, *is_private != 0) {
                    Ok(n) => n,
                    Err(e) => -e as usize,
                };
                Ok(None)
            }

            _ => return Ok(Some(Item::Enarxcall(enarxcall, data))),
        }
    }

    fn handle_vmgexit(&mut self, ghcb_msr: u64, _error: u8) -> Result<()> {
        match ghcb_msr & 0xfff {
            0x000 => {
                // GHCB Guest Physical Address

                self.handle_ghcb_request(ghcb_msr)?;
            }
            0x014 => {
                // SNP Page State Change Request

                self.handle_msr_page_state_request(ghcb_msr)?;
            }
            f => bail!("unimplemented GHCB protocol function {f:#03x}"),
        }

        Ok(())
    }

    fn handle_ghcb_request(&mut self, ghcb_msr: u64) -> Result<(), anyhow::Error> {
        let gfn = ghcb_msr & !0xfff;
        dbg!(format_args!("{gfn:#x}"));

        let mut guard = self.keep.write().unwrap();
        let keep = &mut *guard;

        // Find the memory slot that backs the guest physical address of the
        // GHCB.
        let (slot, map) = keep
            .regions
            .iter_mut()
            .find(|(slot, _)| {
                (slot.guest_phys_addr..slot.guest_phys_addr + slot.memory_size).contains(&gfn)
            })
            .context("can't find GHCB")?;
        let offset = usize::try_from(gfn - slot.guest_phys_addr).unwrap();

        // Create a reference to the GHCB.
        let ghcb_slice = &mut map[offset..][..0x1000];
        let ghcb = unsafe {
            // SAFETY: `Ghcb` is a 0x1000 byte sized struct that's valid for
            // all bit patterns and has no padding bytes.
            // We assume that the guest passes us a unique reference to the
            // memory.
            &mut *(ghcb_slice as *mut [u8] as *mut Ghcb)
        };

        // Validate ghcb protocol.
        ensure!(ghcb.ghcb_usage == 0);
        ensure!(ghcb.protocol_version <= 2);

        match ghcb.save_area.sw_exit_code {
            0x8000_0010 => {
                // SNP Page Stage Change

                // Make sure that the page state change struct is in the shared
                // buffer.
                // The GHCB spec suggests this, but doesn't require it.
                // However, our guest implementation always uses the shared
                // buffer and using that knowledge allows us to simplify the
                // code.
                ensure!(
                    ghcb.save_area.sw_scratch == gfn + 2048,
                    "the page state change struct is not in the shared buffer"
                );

                // Create a reference to the page state change struct in the
                // shared buffer.
                let psc_desc = unsafe {
                    // SAFETY: `SnpPscDesc` is a 2032 byte sized struct that's
                    // valid for all bit patterns and has no padding bytes.
                    &mut *(&mut ghcb.shared_buffer as *mut [u8; 2032] as *mut SnpPscDesc)
                };

                while psc_desc.cur_entry <= psc_desc.end_entry {
                    // Process a page state change.

                    // Extract the individual fields.
                    let entry = psc_desc.entries[usize::from(psc_desc.cur_entry)];
                    let cur_page = entry.entry & 0xfff;
                    let gpa = entry.entry & 0x7_ffff_ffff_f000;
                    let operation = (entry.entry >> 52) & 0xf;
                    let page_size = (entry.entry >> 56) & 1;

                    // Check that the guest requested page state change for a
                    // 4KiB page. We never map 2MiB pages into the guest, so
                    // there's no reason for the guest to request anything else
                    // and for us to support anything else.
                    ensure!(page_size == 0, "request page state change for 2MiB page");
                    ensure!(cur_page == 0);

                    // Try to execute the request.
                    let res = match operation {
                        0x001 => {
                            // Page assignment, Private
                            set_memory_attributes(&mut keep.vm_fd, gpa, 0x1000, true).map_err(
                                |_| {
                                    // Indicate to the guest that an unspecified error occured.
                                    0x0000_0100_0000_0000
                                },
                            )
                        }
                        0x002 => {
                            // Page assignment, Shared
                            set_memory_attributes(&mut keep.vm_fd, gpa, 0x1000, false).map_err(
                                |_| {
                                    // Indicate to the guest that an unspecified error occured.
                                    0x0000_0100_0000_0000
                                },
                            )
                        }
                        0x003 => {
                            // PSMASH hint

                            // We're not required to process the hint.
                            Ok(())
                        }
                        0x004 => {
                            // UNSMASH hint

                            // We're not required to process the hint.
                            Ok(())
                        }
                        _ => {
                            warn!("unimplemented page state change operation {operation:#x}");

                            // Indicate to the guest that the entry is not valid.
                            Err(0x0000_0001_0000_0002)
                        }
                    };

                    // Handle the result.
                    match res {
                        Ok(_) => {
                            psc_desc.cur_entry += 1;
                        }
                        Err(error_code) => {
                            ghcb.save_area.sw_exit_info2 = error_code;
                            break;
                        }
                    }
                }
            }
            _ => {
                bail!("unimplemented sw_exit_code {:#x}", {
                    ghcb.save_area.sw_exit_code
                })
            }
        }

        Ok(())
    }

    fn handle_msr_page_state_request(&mut self, ghcb_msr: u64) -> Result<()> {
        let gpa = ghcb_msr & 0x7_ffff_ffff_f000;
        let page_operation = (ghcb_msr >> 52) & 0xf;

        match page_operation {
            1 => {
                // Page assignment, Private

                set_memory_attributes(&mut self.keep.write().unwrap().vm_fd, gpa, 0x1000, true)
                    .context("failed to change page state to private")?;
            }
            2 => {
                // Page assignment, Shared

                set_memory_attributes(&mut self.keep.write().unwrap().vm_fd, gpa, 0x1000, false)
                    .context("failed to change page state to shared")?;
            }
            _ => bail!("unimplemented operation {page_operation:#x}"),
        }

        Ok(())
    }
}

impl<P: KeepPersonality> super::super::Thread for Thread<P> {
    fn enter(&mut self, _gdblisten: &Option<String>) -> Result<Command> {
        let vcpu_fd = self.vcpu_fd.as_mut().unwrap();
        match vcpu_fd.run()? {
            VcpuExit::IoOut(KVM_SYSCALL_TRIGGER_PORT, data) => {
                debug_assert_eq!(data.len(), 2);
                let block_nr = data[0] as usize + ((data[1] as usize) << 8);
                let block_virt = self.keep.write().unwrap().sallyports[block_nr]
                    .take()
                    .unwrap();

                // If some other thread tried to use the same block, the above unwrap would have panicked.
                let block: Block = unsafe {
                    std::slice::from_raw_parts_mut(
                        block_virt.as_mut_ptr::<usize>(),
                        self.keep.read().unwrap().sallyport_block_size / size_of::<usize>(),
                    )
                }
                .into();

                for item in block {
                    match item {
                        Item::Gdbcall(_gdbcall, _data) => {
                            #[cfg(feature = "gdb")]
                            unsafe {
                                execute_gdb(
                                    _gdbcall,
                                    _data,
                                    &mut self.gdb_fd,
                                    _gdblisten.as_ref().unwrap(),
                                )
                                .map_err(io::Error::from_raw_os_error)
                                .context("execute_gdb")?;
                            }
                        }

                        Item::Enarxcall(enarxcall, data) => {
                            if let Some(Item::Enarxcall(enarxcall, data)) =
                                self.kvm_enarxcall(enarxcall, data)?
                            {
                                let mut keep = self.keep.write().unwrap();
                                sallyport::host::execute(
                                    keep.personality.enarxcall(enarxcall, data)?.into_iter(),
                                )
                                .map_err(io::Error::from_raw_os_error)
                                .context("sallyport::host::execute")?;
                            }
                        }

                        // Catch exit and exit_group for a clean shutdown
                        Item::Syscall(syscall, ..)
                            if (syscall.num == libc::SYS_exit as usize
                                || syscall.num == libc::SYS_exit_group as usize) =>
                        {
                            if cfg!(feature = "dbg") {
                                dbg!(&syscall);
                            }
                            return Ok(Command::Exit(syscall.argv[0] as _));
                        }

                        Item::Syscall(ref _syscall, ..) => {
                            #[cfg(feature = "dbg")]
                            match (
                                _syscall.num as libc::c_long,
                                _syscall.argv[1] as libc::c_int,
                            ) {
                                (
                                    libc::SYS_write | libc::SYS_read,
                                    libc::STDIN_FILENO | libc::STDOUT_FILENO | libc::STDERR_FILENO,
                                ) => {}
                                _ => {
                                    dbg!(&_syscall);
                                }
                            }

                            sallyport::host::execute(iter::once(item))
                                .map_err(io::Error::from_raw_os_error)
                                .context("sallyport::host::execute")?;
                        }
                    }
                }

                self.keep.write().unwrap().sallyports[block_nr].replace(block_virt);
                Ok(Command::Continue)
            }
            VcpuExit::Vmgexit(ghcb_msr, error) => {
                self.handle_vmgexit(ghcb_msr, error)?;
                Ok(Command::Continue)
            }
            #[cfg(debug_assertions)]
            reason => bail!(
                "KVM error: {:?} {:#x?} {:#x?}",
                reason,
                vcpu_fd.get_regs(),
                vcpu_fd.get_sregs()
            ),

            #[cfg(not(debug_assertions))]
            reason => bail!("KVM error: {:?}", reason),
        }
    }
}
