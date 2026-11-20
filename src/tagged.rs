//! Packs an aligned pointer, a generation, and a logical-deletion mark into
//! one machine word.
//!
//! Bit 0 is the Harris/Michael mark: the node is logically deleted. The top
//! 16 bits are a generation for CAS, so the same address showing up again
//! does not look like “the link never changed” (ABA).
//!
//! The remaining bits are the address. The pointer must be aligned to at
//! least 2, and the top 16 bits of the address must be zero. On x86_64 user
//! addresses that is the lower half of the canonical form of a 48-bit
//! virtual address. A node `Box` is aligned more strictly than 2, because
//! it contains atomics.
//!
//! Wrapping the 16-bit generation is a teaching limit. After 2^16 successful
//! CAS operations on the same address the generation repeats, and ABA
//! protection on that address starts over.
//!
//! A null pointer with a non-zero generation is a non-zero word. Emptiness
//! is checked with [`pointer`], not by comparing the word with zero. The
//! stack and the queue leave bit 0 clear. The bucket list in the hash map
//! is what sets it.

const TAG_SHIFT: u32 = 48;
const TAG_MASK: usize = 0xFFFF;
/// Bits 1..=47. Bit 0 and the top 16 bits are not part of the address.
const ADDR_MASK: usize = ((1usize << TAG_SHIFT) - 1) & !1;

const _: () = assert!(usize::BITS == 64, "epoch requires 64-bit pointers");

/// Builds a machine word from an address, a generation, and a deletion mark.
pub fn pack<T>(ptr: *mut T, tag: u16, marked: bool) -> usize {
    let addr = ptr as usize;
    debug_assert_eq!(addr & 1, 0, "pointer must be aligned to at least 2");
    debug_assert_eq!(
        addr >> TAG_SHIFT,
        0,
        "the top 16 address bits are occupied; the generation cannot be packed"
    );
    ((tag as usize) << TAG_SHIFT) | addr | usize::from(marked)
}

/// Address with the generation and the deletion mark stripped off.
pub fn pointer<T>(raw: usize) -> *mut T {
    (raw & ADDR_MASK) as *mut T
}

/// Generation stored in the top 16 bits.
pub fn tag(raw: usize) -> u16 {
    ((raw >> TAG_SHIFT) & TAG_MASK) as u16
}

/// Bit 0: the node is logically deleted from the list.
pub fn is_marked(raw: usize) -> bool {
    raw & 1 == 1
}

/// The same word with the deletion mark set. The address and the generation stay.
pub fn with_mark(raw: usize) -> usize {
    raw | 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_roundtrip_keeps_pointer_tag_and_mark() {
        let boxed = Box::new(7u64);
        let ptr = Box::into_raw(boxed);
        let raw = pack(ptr, 0xAB, true);
        assert!(is_marked(raw));
        assert_eq!(tag(raw), 0xAB);
        assert_eq!(pointer::<u64>(raw), ptr);
        assert_eq!(tag(with_mark(raw)), 0xAB);
        drop(unsafe { Box::from_raw(ptr) });
    }

    #[test]
    fn null_with_tag_is_not_a_null_word() {
        let raw = pack::<u8>(std::ptr::null_mut(), 1, false);
        assert_ne!(raw, 0);
        assert!(pointer::<u8>(raw).is_null());
        assert_eq!(tag(raw), 1);
        assert!(!is_marked(raw));
    }

    #[test]
    fn mark_does_not_change_tag_or_pointer() {
        let ptr = 0x1000 as *mut u8;
        let raw = pack(ptr, 9, false);
        let marked = with_mark(raw);
        assert!(is_marked(marked));
        assert_eq!(tag(marked), 9);
        assert_eq!(pointer::<u8>(marked), ptr);
    }
}
