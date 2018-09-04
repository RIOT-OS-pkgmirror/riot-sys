use raw::{
    gnrc_netif_iter,
    gnrc_netif_t,
    ipv6_addr_t,
    ipv6_addr_from_str,
    kernel_pid_t,
    gnrc_pktsnip_t,
    gnrc_pktbuf_release_error,
    gnrc_pktbuf_hold,
    GNRC_NETERR_SUCCESS,
    gnrc_nettype_t,
    gnrc_ipv6_get_header,
    ipv6_hdr_t,
};

use ::core::iter::Iterator;
use libc;

use core::marker::PhantomData;

struct NetifIter {
    current: *const gnrc_netif_t,
}

impl Iterator for NetifIter {
    type Item = *const gnrc_netif_t;

    fn next(&mut self) -> Option<Self::Item>
    {
        self.current = unsafe { gnrc_netif_iter(self.current) };
        if self.current == 0 as *const gnrc_netif_t {
            None
        } else {
            Some(self.current)
        }
    }
}

pub fn netif_iter() -> impl Iterator<Item = *const gnrc_netif_t> {
    NetifIter { current: 0 as *const gnrc_netif_t }
}

pub struct IPv6Addr
{
    inner: ipv6_addr_t,
}

impl ::core::str::FromStr for IPv6Addr
{
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // It'd be nice to use std::net::IPv6Addr::from_str, but the parser is generic over
        // families (maybe at some point we'll need that here too, but not now), and it's in std
        // rather then core for reasons I can't really follow.

        let s = s.as_bytes();

        let mut with_null = [0u8; 32 + 7 + 1]; // 32 nibbles + 7 colons + null byte
        if s.len() > with_null.len() - 1 {
            // Obviously too long to be a valid plain address
            return Err(())
        }
        with_null[..s.len()].copy_from_slice(s);

        // FIXME: use MaybeUninit when available
        let mut ret: Self = Self { inner: ipv6_addr_t { u8: [0; 16]} };

        let conversion_result = unsafe { ipv6_addr_from_str(&mut ret.inner, libc::CStr::from_bytes_with_nul_unchecked(&with_null).as_ptr()) };

        match conversion_result as usize {
            0 => Err(()),
            _ => Ok(ret),
        }
    }
}

impl ::core::fmt::Debug for IPv6Addr
{
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        let as_u8 = unsafe { &self.inner.u8 };
        write!(f, "{:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}:\
            {:02x}{:02x}:{:02x}{:02x}",
            as_u8[0],
            as_u8[1],
            as_u8[2],
            as_u8[3],
            as_u8[4],
            as_u8[5],
            as_u8[6],
            as_u8[7],
            as_u8[8],
            as_u8[9],
            as_u8[10],
            as_u8[11],
            as_u8[12],
            as_u8[13],
            as_u8[14],
            as_u8[15], 
            )
    }
}

impl IPv6Addr
{
    pub unsafe fn as_ptr(&self) -> *const ipv6_addr_t {
        &self.inner
    }
}

/// Given an address like fe80::1%42, split it up into a IPv6Addr and a numeric interface
/// identifier, if any is given. It is an error for the address not to be parsable, or for the
/// interface identifier not to be numeric.
///
/// Don't consider the error type final, that's just what works easily Right Now.
// This is not implemented in terms of the RIOT ipv6_addr functions as they heavily rely on
// null-terminated strings and mutating memory.
pub fn split_ipv6_address(input: &str) -> Result<(IPv6Addr, Option<kernel_pid_t>), &'static str> {
    let mut s = input.splitn(2, "%");
    let addr = s.next()
        .ok_or("No address")?
        .parse()
        .map_err(|_| "Unparsable address")?;
    let interface = match s.next() {
        None => None,
        Some(x) => Some(x.parse().map_err(|_| "Non-numeric interface identifier")?)
    };

    Ok((addr, interface))
}

#[derive(Debug)]
pub struct PktsnipPart<'a> {
    data: &'a [u8],
    type_: gnrc_nettype_t,
}

pub struct SnipIter<'a> {
    pointer: *const gnrc_pktsnip_t,
    datalifetime: PhantomData<&'a ()>,
}

impl<'a> Iterator for SnipIter<'a> {
    type Item = PktsnipPart<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let s = self.pointer;
        if s == 0 as *const _ {
            return None
        }
        let s = unsafe { *s };
        self.pointer = s.next;
        Some(PktsnipPart {
            data: unsafe { ::core::slice::from_raw_parts(::core::mem::transmute(s.data), s.size) },
            type_: s.type_
        })
    }
}

/// Wrapper type around gnrc_pktsnip_t that takes care of the reference counting involved.
pub struct Pktsnip(*mut gnrc_pktsnip_t);

/// Pktsnip can be send because any volatile fields are accessed through the appropriate functions
/// (hold, release), and the non-volatile fields are only written to by threads that made sure they
/// obtained a COW copy using start_write.
unsafe impl Send for Pktsnip {}

impl From<*mut gnrc_pktsnip_t> for Pktsnip {
    /// Accept this pointer as the refcounting wrapper's responsibility
    fn from(input: *mut gnrc_pktsnip_t) -> Self {
        Pktsnip(input)
    }
}

impl Clone for Pktsnip {
    fn clone(&self) -> Pktsnip {
        unsafe { gnrc_pktbuf_hold(self.0, 1) };
        Pktsnip(self.0)
    }
}

impl Drop for Pktsnip {
    fn drop(&mut self) {
        unsafe { gnrc_pktbuf_release_error(self.0, GNRC_NETERR_SUCCESS) }
    }
}

impl Pktsnip {
    pub fn len(&self) -> usize {
        // Implementing the static function gnrc_pkt_len
        self.iter_snips().map(|s| s.data.len()).sum()
    }

    pub fn count(&self) -> usize {
        // Implementing the static function gnrc_pkt_count
        self.iter_snips().count()
    }

    pub fn get_ipv6_hdr(&self) -> Option<&ipv6_hdr_t> {
        let hdr = unsafe { gnrc_ipv6_get_header(self.0) };
        if hdr == 0 as *mut _ {
            None
        } else {
            // It's OK to hand out a reference: self.0 is immutable in its data areas, and hdr
            // should point somewhere in there
            Some(unsafe { &*hdr })
        }
    }

    pub fn iter_snips(&self) -> SnipIter {
        SnipIter { pointer: self.0, datalifetime: PhantomData }
    }
}

impl ::core::fmt::Debug for Pktsnip {
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        write!(f, "Pktsnip {{ length {}, in {} snips }}", self.len(), self.count())
    }
}
