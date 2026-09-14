use super::store_column_width;

#[test]
fn store_column_width_uses_longest_name_plus_two() {
    assert_eq!(store_column_width(["a", "bb", "ccc"].into_iter()), 5);
    assert_eq!(store_column_width(std::iter::empty()), 2);
}
