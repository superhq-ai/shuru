use std::io;

pub struct Superblock {
    pub blocks: u64,
    pub errors: u32,
    pub needs_recovery: bool,
}

pub fn parse(sb: &[u8; 1024]) -> io::Result<Superblock> {
    let u16_at = |n| u16::from_le_bytes(sb[n..n + 2].try_into().unwrap());
    let u32_at = |n| u32::from_le_bytes(sb[n..n + 4].try_into().unwrap());
    if u16_at(56) != 0xef53 {
        return Err(io::Error::other("root device is not ext4"));
    }
    if u16_at(58) & 2 != 0 {
        return Err(io::Error::other("root ext4 EXT4_ERROR_FS flag is set"));
    }
    let incompat = u32_at(96);
    let high = if incompat & 0x80 != 0 {
        u32_at(336) as u64
    } else {
        0
    };
    Ok(Superblock {
        blocks: u32_at(4) as u64 | (high << 32),
        errors: u32_at(404),
        needs_recovery: incompat & 4 != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_dirty_disks_are_allowed_but_error_flags_are_not() {
        let mut sb = [0; 1024];
        assert!(parse(&sb).is_err());
        sb[56..58].copy_from_slice(&0xef53u16.to_le_bytes());
        sb[96] = 4;
        assert!(parse(&sb).unwrap().needs_recovery);
        sb[58] = 2;
        assert!(parse(&sb)
            .err()
            .unwrap()
            .to_string()
            .contains("EXT4_ERROR_FS"));
    }

    #[test]
    fn high_block_count_requires_64bit_feature() {
        let mut sb = [0; 1024];
        sb[56..58].copy_from_slice(&0xef53u16.to_le_bytes());
        sb[4] = 1;
        sb[336] = 2;
        assert_eq!(parse(&sb).unwrap().blocks, 1);
        sb[96] = 0x80;
        assert_eq!(parse(&sb).unwrap().blocks, (2u64 << 32) + 1);
        sb[404] = 3;
        assert_eq!(parse(&sb).unwrap().errors, 3);
    }
}
