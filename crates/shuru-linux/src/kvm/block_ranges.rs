pub(super) const VIRTIO_BLK_T_IN: u32 = 0;
pub(super) const VIRTIO_BLK_T_OUT: u32 = 1;
pub(super) const VIRTIO_BLK_T_DISCARD: u32 = 11;
pub(super) const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13;
pub(super) const VIRTIO_BLK_S_UNSUPP: u8 = 2;
pub(super) const MAX_RANGES: usize = 32;
pub(super) const MAX_RANGE_SECTORS: u32 = 2 * 1024 * 1024;

pub(super) fn decode_ranges(
    payload: &[u8],
    capacity: u64,
    kind: u32,
) -> Result<Vec<(i64, i64, bool)>, u8> {
    if payload.is_empty() || payload.len() % 16 != 0 || payload.len() / 16 > MAX_RANGES {
        return Err(VIRTIO_BLK_S_IOERR);
    }
    let mut ranges = Vec::new();
    for bytes in payload.chunks_exact(16) {
        let sector = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let flags = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
        if (kind == VIRTIO_BLK_T_DISCARD && flags != 0) || flags & !1 != 0 {
            return Err(VIRTIO_BLK_S_UNSUPP);
        }
        if count > MAX_RANGE_SECTORS || sector > capacity || count as u64 > capacity - sector {
            return Err(VIRTIO_BLK_S_IOERR);
        }
        let end = sector
            .checked_add(count as u64)
            .and_then(|s| s.checked_mul(512))
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or(VIRTIO_BLK_S_IOERR)?;
        let len = count as i64 * 512;
        ranges.push((end as i64 - len, len, flags & 1 != 0));
    }
    Ok(ranges)
}

pub(super) const VIRTIO_BLK_S_OK: u8 = 0;
pub(super) const VIRTIO_BLK_S_IOERR: u8 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    fn range(sector: u64, count: u32, flags: u32) -> Vec<u8> {
        [
            sector.to_le_bytes().as_slice(),
            count.to_le_bytes().as_slice(),
            flags.to_le_bytes().as_slice(),
        ]
        .concat()
    }

    #[test]
    fn validates_all_ranges_and_signed_host_offsets() {
        assert_eq!(
            decode_ranges(&range(8, 8, 1), 16, VIRTIO_BLK_T_WRITE_ZEROES),
            Ok(vec![(4096, 4096, true)])
        );
        assert_eq!(
            decode_ranges(&range(8, 9, 0), 16, VIRTIO_BLK_T_DISCARD),
            Err(VIRTIO_BLK_S_IOERR)
        );
        assert_eq!(
            decode_ranges(&range(u64::MAX, 1, 0), u64::MAX, VIRTIO_BLK_T_DISCARD),
            Err(VIRTIO_BLK_S_IOERR)
        );
        assert_eq!(
            decode_ranges(
                &range(i64::MAX as u64 / 512, 1, 0),
                u64::MAX,
                VIRTIO_BLK_T_DISCARD
            ),
            Err(VIRTIO_BLK_S_IOERR)
        );
        let mut payload = range(0, 8, 0);
        payload.extend(range(15, 2, 0));
        assert_eq!(
            decode_ranges(&payload, 16, VIRTIO_BLK_T_DISCARD),
            Err(VIRTIO_BLK_S_IOERR)
        );
    }

    #[test]
    fn enforces_segment_count_length_flags_and_limits() {
        for payload in [
            vec![],
            vec![0; 15],
            vec![0; (MAX_RANGES + 1) * 16],
            range(0, MAX_RANGE_SECTORS + 1, 0),
        ] {
            assert_eq!(
                decode_ranges(&payload, u64::MAX, VIRTIO_BLK_T_DISCARD),
                Err(VIRTIO_BLK_S_IOERR)
            );
        }
        assert_eq!(
            decode_ranges(&range(0, 8, 1), 16, VIRTIO_BLK_T_DISCARD),
            Err(VIRTIO_BLK_S_UNSUPP)
        );
        assert_eq!(
            decode_ranges(&range(0, 8, 2), 16, VIRTIO_BLK_T_WRITE_ZEROES),
            Err(VIRTIO_BLK_S_UNSUPP)
        );
        assert!(decode_ranges(&vec![0; MAX_RANGES * 16], 16, VIRTIO_BLK_T_DISCARD).is_ok());
        assert!(decode_ranges(&range(16, 0, 0), 16, VIRTIO_BLK_T_DISCARD).is_ok());
    }
}
