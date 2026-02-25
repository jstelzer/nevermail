/// melib::Flag::SEEN  = 0b0000_0001
/// melib::Flag::FLAGGED = 0b0100_0000  (but we store our own compact encoding)
/// We use a simple two-bit encoding for the flags we care about:
///   bit 0 = SEEN
///   bit 1 = FLAGGED
pub fn flags_to_u8(is_read: bool, is_starred: bool) -> u8 {
    let mut f: u8 = 0;
    if is_read {
        f |= 1;
    }
    if is_starred {
        f |= 2;
    }
    f
}

pub fn flags_from_u8(f: u8) -> (bool, bool) {
    (f & 1 != 0, f & 2 != 0)
}
