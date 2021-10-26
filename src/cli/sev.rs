// SPDX-License-Identifier: Apache-2.0

use std::io;

use crate::backend::sev::certs::*;
use crate::backend::sev::Firmware;

use anyhow::{anyhow, Context, Result};
use openssl::x509::X509;
use structopt::StructOpt;

fn merge_vcek_stack(vcek_der: &[u8], chain_pem: &str) -> Result<String> {
    let vcek_pem = X509::from_der(vcek_der)
        .context("failed to parse VCEK certificate")?
        .to_pem()
        .context("failed to format VCEK certificate as PEM")
        .map(String::from_utf8)?
        .context("invalid PEM generated by openssl")?;
    Ok(format!("{}{}", vcek_pem, chain_pem))
}

fn write_vcek<T: io::Write>(w: &mut T) -> Result<()> {
    let mut sev = Firmware::open().context("failed to open SEV device")?;

    let id = sev.identifier().context("failed to query SEV identifier")?;

    let status = sev
        .platform_status()
        .context("failed to query SEV platform status")?;
    if status.tcb.platform_version != status.tcb.reported_version {
        // It is not clear from the documentation what the difference between the two is,
        // therefore only proceed if they are identical to ensure correctness.
        // TODO: Figure out which one should be used and drop this check.
        return Err(anyhow!(
            "reported TCB version is not equal to installed TCB version"
        ));
    }

    let client = reqwest::blocking::Client::new();

    let vcek_der = client
        .get(vcek_url(id, status.tcb.reported_version))
        .send()
        .context("failed to GET VCEK certificate")?
        .bytes()
        .context("failed to read VCEK certificate GET response bytes")?;

    let chain_pem = client
        .get(CHAIN_URL)
        .send()
        .context("failed to GET VCEK certificate chain")?
        .text()
        .context("failed to read VCEK certificate chain GET response text")?;

    let stack_pem = merge_vcek_stack(&vcek_der, &chain_pem)?;
    write!(w, "{}", stack_pem)?;
    Ok(())
}

/// SEV-specific functionality
#[derive(StructOpt, Debug)]
pub enum Command {
    /// Download VCEK certificates for SEV platform and print to stdout in PEM format
    Vcek,
}

pub fn run(cmd: Command) -> Result<()> {
    match cmd {
        Command::Vcek => write_vcek(&mut io::stdout()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_vcek_stack() -> Result<()> {
        let pem = merge_vcek_stack(
            include_bytes!("testdata/vcek.der"),
            include_str!("testdata/chain.pem"),
        )?;
        let certs = X509::stack_from_pem(pem.as_bytes())?;
        assert_eq!(certs.len(), 3);
        let cert1_key = certs[1].public_key()?;
        let cert2_key = certs[2].public_key()?;
        assert!(certs[0].verify(&cert1_key).expect(
            "failed to verify that certificate 0 is signed using public key of certificate 1"
        ));
        assert!(certs[1].verify(&cert2_key).expect(
            "failed to verify that certificate 1 is signed using public key of certificate 2"
        ));
        Ok(())
    }
}