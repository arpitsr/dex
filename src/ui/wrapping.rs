use unicode_width::UnicodeWidthChar;

/// Greedily wrap an input line and locate the cursor in the resulting rows.
/// `col` and the returned cursor position use bytes and display cells
/// respectively, matching the input editor and ratatui.
pub(super) fn wrap_line(line: &str, width: usize, col: usize) -> (Vec<String>, u16, u16) {
    let width = width.max(1);
    let col = col.min(line.len());
    let bounds: Vec<usize> = line
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(line.len()))
        .collect();

    let mut segments = Vec::new();
    let mut start = 0;
    let mut row_width = 0;
    let mut last_space_end = None;

    for index in 0..bounds.len().saturating_sub(1) {
        let begin = bounds[index];
        let end = bounds[index + 1];
        let ch = line[begin..end].chars().next().unwrap();
        let char_width = ch.width().unwrap_or(0).max(1);

        if ch.is_whitespace() {
            last_space_end = Some(end);
        }
        if row_width + char_width > width && begin > start {
            if let Some(space_end) = last_space_end.filter(|end| *end > start) {
                segments.push((start, space_end));
                start = space_end;
                row_width = line[start..begin]
                    .chars()
                    .map(|c| c.width().unwrap_or(0).max(1))
                    .sum();
            } else {
                segments.push((start, begin));
                start = begin;
                row_width = 0;
            }
            last_space_end = None;
        }
        row_width += char_width;
    }
    segments.push((start, line.len()));

    let strings = segments
        .iter()
        .map(|&(start, end)| line[start..end].to_string())
        .collect();

    let mut cursor_segment = segments.len().saturating_sub(1) as u16;
    let mut cursor_x = 0;
    for (index, &(start, end)) in segments.iter().enumerate() {
        if col >= start && col < end || (col == end && index == segments.len() - 1) {
            cursor_segment = index as u16;
            cursor_x = line[start..col]
                .chars()
                .map(|c| c.width().unwrap_or(0).max(1))
                .sum::<usize>() as u16;
            break;
        }
    }
    (strings, cursor_segment, cursor_x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn wraps_at_whitespace_and_tracks_cursor() {
        let (lines, row, column) = wrap_line("one two", 5, "one two".len());
        assert_eq!(lines, vec!["one ", "two"]);
        assert_eq!((row, column), (1, 3));
    }

    #[test]
    fn uses_display_width_for_unicode() {
        let (lines, row, column) = wrap_line("ab界d", 3, "ab界".len());
        assert_eq!(lines, vec!["ab", "界d"]);
        assert_eq!((row, column), (1, 2));
        assert_eq!(UnicodeWidthStr::width(lines[1].as_str()), 3);
    }

    #[test]
    fn narrow_width_still_makes_progress() {
        let (lines, _, _) = wrap_line("abc", 0, 0);
        assert_eq!(lines, vec!["a", "b", "c"]);
    }
}
