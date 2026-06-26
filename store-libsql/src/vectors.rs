use std::fmt::Write;

use localdb_core::VectorEncoding;

/// Binarize an f32 embedding vector into packed bytes (MSB-first).
///
/// Each element: `>= 0.0` → bit 1, `< 0.0` → bit 0.
/// A 1024-dim vector becomes 128 bytes.
pub fn binarize_msb(v: &[f32]) -> Vec<u8> {
    v.chunks(8)
        .map(|chunk| {
            let mut byte = 0u8;
            for (i, &val) in chunk.iter().enumerate() {
                if val >= 0.0 {
                    byte |= 1 << (7 - i);
                }
            }
            byte
        })
        .collect()
}

/// Format an f32 vector as a SQL literal for `vector32('[...]')`.
pub fn f32_to_vector32_sql(v: &[f32]) -> String {
    let mut s = String::from("vector32('[");
    for (i, &val) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{val}");
    }
    s.push_str("]')");
    s
}

/// Format an f32 vector as a binarized SQL literal for `vector1bit('[...]')`.
///
/// Applies `binarize_msb` first, then formats each bit as 0/1 in the SQL literal.
pub fn f32_to_vector1bit_sql(v: &[f32]) -> String {
    let bytes = binarize_msb(v);
    let dim = v.len();
    let mut s = String::from("vector1bit('[");
    let mut bit_index = 0;
    for byte in &bytes {
        for bit_pos in (0..8).rev() {
            if bit_index >= dim {
                break;
            }
            if bit_index > 0 {
                s.push(',');
            }
            let bit = (byte >> bit_pos) & 1;
            let _ = write!(s, "{bit}");
            bit_index += 1;
        }
    }
    s.push_str("]')");
    s
}

/// Format a raw query vector for the appropriate encoding.
#[allow(dead_code)] // used in Wave 4 (search methods)
pub fn query_vector_sql(v: &[f32], encoding: VectorEncoding) -> String {
    match encoding {
        VectorEncoding::Float32 => f32_to_vector32_sql(v),
        VectorEncoding::Binary => f32_to_vector1bit_sql(v),
    }
}

/// Return the SQL column type for the embedding column.
pub fn embedding_column_type(dim: usize, encoding: VectorEncoding) -> String {
    match encoding {
        VectorEncoding::Float32 => format!("F32_BLOB({dim})"),
        VectorEncoding::Binary => format!("F1BIT_BLOB({dim})"),
    }
}

/// Convert a cosine distance (from `vector_distance_cos`) to a similarity score [0, 1].
///
/// Cosine distance = 1 - cosine_similarity, range [0, 2].
#[allow(dead_code)] // used in Wave 4 (dense_search)
pub fn cosine_distance_to_score(distance: f64) -> f32 {
    (1.0 - distance / 2.0) as f32
}

/// Convert a Hamming distance to a similarity score [0, 1].
///
/// Hamming distance = number of differing bits. Range [0, nbits].
#[allow(dead_code)] // used in Wave 4 (dense_search with Binary encoding)
pub fn hamming_distance_to_score(distance: f64, nbits: usize) -> f32 {
    (1.0 - distance / nbits as f64) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binarize_msb_basic() {
        // All positive → all 1s
        let all_pos = [1.0f32; 8];
        assert_eq!(binarize_msb(&all_pos), vec![0xFF]);

        // All negative → all 0s
        let all_neg = [-1.0f32; 8];
        assert_eq!(binarize_msb(&all_neg), vec![0x00]);
    }

    #[test]
    fn test_binarize_msb_mixed() {
        // Alternating positive/negative → 10101010 = 0xAA = 170
        let mixed = [1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0, -1.0];
        assert_eq!(binarize_msb(&mixed), vec![0xAA]);

        // Two bytes of the same pattern
        let mixed16: Vec<f32> = mixed.iter().cycle().take(16).copied().collect();
        assert_eq!(binarize_msb(&mixed16), vec![0xAA, 0xAA]);
    }

    #[test]
    fn test_binarize_msb_zero_is_positive() {
        // 0.0 should map to bit 1 (>= 0.0)
        let zeros = [0.0f32; 8];
        assert_eq!(binarize_msb(&zeros), vec![0xFF]);
    }

    #[test]
    fn test_binarize_partial_byte() {
        // 5 values: only first 5 bits used, last 3 bits are 0
        // [1.0, -1.0, 1.0, -1.0, 1.0] → 10101_000 = 0xA8 = 168
        let v = [1.0f32, -1.0, 1.0, -1.0, 1.0];
        assert_eq!(binarize_msb(&v), vec![0b1010_1000]);

        // 1 value positive → 1_0000000 = 0x80
        assert_eq!(binarize_msb(&[1.0f32]), vec![0x80]);
    }

    #[test]
    fn test_f32_to_vector32_sql() {
        let v = [0.1f32, 0.2, 0.3];
        let sql = f32_to_vector32_sql(&v);
        assert_eq!(sql, "vector32('[0.1,0.2,0.3]')");

        // Single element
        let sql = f32_to_vector32_sql(&[1.5]);
        assert_eq!(sql, "vector32('[1.5]')");

        // Empty
        let sql = f32_to_vector32_sql(&[]);
        assert_eq!(sql, "vector32('[]')");
    }

    #[test]
    fn test_f32_to_vector1bit_sql() {
        // 8 values, alternating → bits: 1,0,1,0,1,0,1,0
        let v = [1.0f32, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0, -1.0];
        let sql = f32_to_vector1bit_sql(&v);
        assert_eq!(sql, "vector1bit('[1,0,1,0,1,0,1,0]')");

        // All positive → all 1s
        let v = [1.0f32; 4];
        let sql = f32_to_vector1bit_sql(&v);
        assert_eq!(sql, "vector1bit('[1,1,1,1]')");

        // Partial byte: 5 values, verify we trim to exactly 5 bits
        let v = [1.0f32, -1.0, 1.0, -1.0, 1.0];
        let sql = f32_to_vector1bit_sql(&v);
        assert_eq!(sql, "vector1bit('[1,0,1,0,1]')");
    }

    #[test]
    fn test_embedding_column_type() {
        assert_eq!(
            embedding_column_type(1024, VectorEncoding::Float32),
            "F32_BLOB(1024)"
        );
        assert_eq!(
            embedding_column_type(1024, VectorEncoding::Binary),
            "F1BIT_BLOB(1024)"
        );
        assert_eq!(
            embedding_column_type(384, VectorEncoding::Float32),
            "F32_BLOB(384)"
        );
    }

    #[test]
    fn test_cosine_distance_to_score() {
        // distance 0.0 → score 1.0 (identical vectors)
        assert!((cosine_distance_to_score(0.0) - 1.0).abs() < f32::EPSILON);
        // distance 2.0 → score 0.0 (opposite vectors)
        assert!((cosine_distance_to_score(2.0) - 0.0).abs() < f32::EPSILON);
        // distance 1.0 → score 0.5 (orthogonal)
        assert!((cosine_distance_to_score(1.0) - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_hamming_distance_to_score() {
        // 0 bits differ out of 64 → score 1.0
        assert!((hamming_distance_to_score(0.0, 64) - 1.0).abs() < f32::EPSILON);
        // All bits differ → score 0.0
        assert!((hamming_distance_to_score(64.0, 64) - 0.0).abs() < f32::EPSILON);
        // Half differ → score 0.5
        assert!((hamming_distance_to_score(32.0, 64) - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_query_vector_sql_dispatches_correctly() {
        let v = [0.5f32, -0.5];
        let f32_sql = query_vector_sql(&v, VectorEncoding::Float32);
        assert!(f32_sql.starts_with("vector32("));

        let bin_sql = query_vector_sql(&v, VectorEncoding::Binary);
        assert!(bin_sql.starts_with("vector1bit("));
    }
}
