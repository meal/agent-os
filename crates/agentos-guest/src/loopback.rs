//! The loopback interface, for the model proxy (VM only). The VM has no `ip` binary, so this
//! does what `ip link set lo up|down` does: SIOCGIFFLAGS, then SIOCSIFFLAGS on an AF_INET
//! datagram socket (netdevice(7)). Setting flags needs CAP_NET_ADMIN, so only reading them is
//! testable on the host; the KVM tier covers the rest.

use rustix::io::Errno;
use rustix::ioctl::{Opcode, Setter, Updater, ioctl};
use rustix::net::{AddressFamily, SocketType, socket};

const SIOCGIFFLAGS: Opcode = 0x8913;
const SIOCSIFFLAGS: Opcode = 0x8914;
/// `IFF_UP` and `IFF_LOOPBACK` from `<linux/if.h>`.
pub const IFF_UP: i16 = 0x1;
pub const IFF_LOOPBACK: i16 = 0x8;

/// `struct ifreq` on 64-bit Linux: a 16-byte interface name, then a 24-byte union whose first
/// member used here is `short ifr_flags` at offset 16. 40 bytes in all.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Ifreq {
    name: [u8; 16],
    flags: i16,
    _union_rest: [u8; 22],
}

impl Ifreq {
    fn named(name: &str) -> Ifreq {
        let mut req = Ifreq {
            name: [0; 16],
            flags: 0,
            _union_rest: [0; 22],
        };
        // Names longer than 15 bytes never reach the kernel; `lo` is two.
        let n = name.len().min(15);
        req.name[..n].copy_from_slice(&name.as_bytes()[..n]);
        req
    }
}

fn control_socket() -> Result<rustix::fd::OwnedFd, Errno> {
    socket(AddressFamily::INET, SocketType::DGRAM, None)
}

fn read_flags(fd: &rustix::fd::OwnedFd, req: &mut Ifreq) -> Result<(), Errno> {
    // SAFETY: SIOCGIFFLAGS reads `ifr_name` and writes `ifr_flags` inside a `struct ifreq`,
    // which `Ifreq` mirrors (repr(C), 40 bytes, checked by a test).
    unsafe { ioctl(fd, Updater::<SIOCGIFFLAGS, Ifreq>::new(req))? };
    Ok(())
}

/// The flags of the interface `name` (`IFF_UP`, `IFF_LOOPBACK`, ...).
pub fn flags(name: &str) -> Result<i16, Errno> {
    let fd = control_socket()?;
    let mut req = Ifreq::named(name);
    read_flags(&fd, &mut req)?;
    Ok(req.flags)
}

/// Brings `lo` up or down, keeping its other flags.
pub fn set_loopback(up: bool) -> Result<(), Errno> {
    let fd = control_socket()?;
    let mut req = Ifreq::named("lo");
    read_flags(&fd, &mut req)?;
    if up {
        req.flags |= IFF_UP;
    } else {
        req.flags &= !IFF_UP;
    }
    // SAFETY: SIOCSIFFLAGS takes the same `struct ifreq` as input.
    unsafe { ioctl(&fd, Setter::<SIOCSIFFLAGS, Ifreq>::new(req))? };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ifreq_has_the_kernel_layout() {
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<Ifreq>(), 40);
        let req = Ifreq::named("lo");
        assert_eq!(&req.name[..3], b"lo\0");
    }

    #[test]
    fn the_loopback_interface_reports_its_flags() {
        // Works without CAP_NET_ADMIN: reading flags is unprivileged.
        let flags = flags("lo").expect("SIOCGIFFLAGS on lo");
        assert_ne!(
            flags & IFF_LOOPBACK,
            0,
            "lo must be a loopback device: {flags:#x}"
        );
    }
}
