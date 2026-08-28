pub(crate) fn is_mutating(name: &str) -> bool {
    super::metadata(name)
        .map(|metadata| metadata.mutating)
        .unwrap_or(true)
}
