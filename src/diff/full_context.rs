use super::hunk_line_starts;

/// Fill omitted unchanged rows from the exact new-file contents.
/// A mismatch leaves the caller's original patch intact instead of inventing edits.
pub fn expand_full_context(patch: &str, head: &str) -> Option<String> {
    if head.contains('\0')
        || patch
            .lines()
            .take_while(|line| !line.starts_with("@@"))
            .any(|line| line == "GIT binary patch" || line.starts_with("Binary files "))
    {
        return None;
    }
    let head_lines: Vec<&str> = head.lines().collect();
    let patch_lines: Vec<&str> = patch.lines().collect();
    let first_hunk = patch_lines.iter().position(|line| line.starts_with("@@"));
    let metadata_end = first_hunk.unwrap_or(patch_lines.len());
    let mut body = String::with_capacity(head.len() + patch.len());
    let mut old_cursor = 0;
    let mut new_cursor = 0;
    let mut index = metadata_end;

    while index < patch_lines.len() {
        let header = patch_lines[index];
        let (old_start, new_start) = hunk_line_starts(header)?;
        let (old_count, new_count) = hunk_counts(header)?;
        let old_offset = range_offset(old_start, old_count)?;
        let new_offset = range_offset(new_start, new_count)?;
        let gap = new_offset.checked_sub(new_cursor)?;
        if old_cursor + gap != old_offset || new_offset > head_lines.len() {
            return None;
        }
        for line in &head_lines[new_cursor..new_offset] {
            append_row(&mut body, ' ', line);
        }
        old_cursor += gap;
        new_cursor = new_offset;
        index += 1;

        while let Some(line) = patch_lines.get(index) {
            if line.starts_with("@@") {
                break;
            }
            match line.as_bytes().first() {
                Some(b'+') | Some(b' ') => {
                    if head_lines.get(new_cursor).copied() != Some(&line[1..]) {
                        return None;
                    }
                    new_cursor += 1;
                    if line.starts_with(' ') {
                        old_cursor += 1;
                    }
                }
                Some(b'-') => old_cursor += 1,
                Some(b'\\') if *line == "\\ No newline at end of file" => {}
                _ => return None,
            }
            body.push_str(line);
            body.push('\n');
            index += 1;
        }
        if old_cursor != old_offset + old_count || new_cursor != new_offset + new_count {
            return None;
        }
    }

    let tail = head_lines.get(new_cursor..)?;
    for line in tail {
        append_row(&mut body, ' ', line);
    }
    let old_count = old_cursor + tail.len();
    let new_count = head_lines.len();
    if old_count == 0 && new_count == 0 && first_hunk.is_none() {
        return Some(patch.to_owned());
    }
    let mut expanded = String::with_capacity(body.len() + metadata_end * 40 + 50);
    for line in &patch_lines[..metadata_end] {
        expanded.push_str(line);
        expanded.push('\n');
    }
    expanded.push_str(&format!(
        "@@ -{},{old_count} +{},{new_count} @@\n",
        usize::from(old_count > 0),
        usize::from(new_count > 0),
    ));
    expanded.push_str(body.strip_suffix('\n').unwrap_or(&body));
    Some(expanded)
}

fn append_row(output: &mut String, marker: char, line: &str) {
    output.push(marker);
    output.push_str(line);
    output.push('\n');
}

fn range_offset(start: u32, count: usize) -> Option<usize> {
    if count == 0 {
        Some(start as usize)
    } else {
        start.checked_sub(1).map(|start| start as usize)
    }
}

fn hunk_counts(header: &str) -> Option<(usize, usize)> {
    let mut parts = header.strip_prefix("@@ ")?.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    if parts.next()? != "@@" {
        return None;
    }
    Some((range_count(old)?, range_count(new)?))
}

fn range_count(range: &str) -> Option<usize> {
    match range.split_once(',') {
        Some((_, count)) => count.parse().ok(),
        None => Some(1),
    }
}

#[cfg(test)]
mod tests {
    use super::expand_full_context;
    use crate::diff::{find_diff_line_index, get_line_info, SideLine};

    #[test]
    fn loading_two_pr_hunks_shows_the_entire_head_and_preserves_removed_lines() {
        let patch = "@@ -2,2 +2,2 @@\n before\n-old\n+new\n@@ -6 +6 @@\n-old six\n+new six";
        let head = "first\nbefore\nnew\nfour\nfive\nnew six\nlast\n";
        let expanded = expand_full_context(patch, head).unwrap();
        insta::assert_snapshot!(expanded, @r"
        @@ -1,7 +1,7 @@
         first
         before
        -old
        +new
         four
         five
        -old six
        +new six
         last
        ");
        for line in 1..=7 {
            let index = find_diff_line_index(&expanded, SideLine::right(line)).unwrap();
            assert_eq!(
                get_line_info(&expanded, index).unwrap().new_line_number,
                Some(line)
            );
        }
        assert_eq!(find_diff_line_index(&expanded, SideLine::left(3)), Some(3));
        assert_eq!(find_diff_line_index(&expanded, SideLine::left(6)), Some(7));
    }

    #[test]
    fn zero_length_hunk_ranges_work_at_file_boundaries() {
        let scenarios = [
            (
                "@@ -0,0 +1 @@\n+first",
                "first\nlast\n",
                "@@ -1,1 +1,2 @@\n+first\n last",
            ),
            (
                "@@ -2 +1,0 @@\n-last",
                "first\n",
                "@@ -1,2 +1,1 @@\n first\n-last",
            ),
            (
                "@@ -0,0 +1,2 @@\n+one\n+two",
                "one\ntwo\n",
                "@@ -0,0 +1,2 @@\n+one\n+two",
            ),
            (
                "@@ -1,2 +0,0 @@\n-one\n-two",
                "",
                "@@ -1,2 +0,0 @@\n-one\n-two",
            ),
        ];
        for (patch, head, expected) in scenarios {
            assert_eq!(
                expand_full_context(patch, head).as_deref(),
                Some(expected),
                "{patch}"
            );
        }
    }

    #[test]
    fn pure_rename_and_mode_changes_show_full_text() {
        let patch = "diff --git a/old.rs b/new.rs\nsimilarity index 100%\nrename from old.rs\nrename to new.rs";
        insta::assert_snapshot!(expand_full_context(patch, "α\n\n終\n").unwrap(), @"
        diff --git a/old.rs b/new.rs
        similarity index 100%
        rename from old.rs
        rename to new.rs
        @@ -1,3 +1,3 @@
         α
        \x20
         終
        ");
    }

    #[test]
    fn mismatched_or_incomplete_patches_and_binary_content_are_rejected() {
        for (patch, head) in [
            ("@@ -1 +1 @@\n-old\n+new", "different\n"),
            ("@@ -1,2 +1,2 @@\n-old\n+new", "new\nlast\n"),
            ("@@ -4 +2 @@\n-old\n+new", "first\nnew\n"),
            ("@@ -1 +1 @@\n-old\n+new", "new\0\n"),
        ] {
            assert!(expand_full_context(patch, head).is_none(), "{patch:?}");
        }
    }

    #[test]
    fn missing_final_newlines_do_not_shift_new_file_line_numbers() {
        let patch =
            "@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file";
        let expanded = expand_full_context(patch, "new").unwrap();
        let index = find_diff_line_index(&expanded, SideLine::right(1)).unwrap();
        assert_eq!(
            get_line_info(&expanded, index).unwrap().new_line_number,
            Some(1)
        );
    }

    #[test]
    fn source_text_mentioning_binary_patch_metadata_is_still_text() {
        let patch = "@@ -1 +1 @@\n-old\n+Binary files and GIT binary patch";
        assert!(
            expand_full_context(patch, "Binary files and GIT binary patch\nlast\n")
                .unwrap()
                .ends_with(" last")
        );
    }

    #[test]
    fn insertions_and_deletions_between_pr_hunks_keep_both_side_numbers() {
        let patch =
            "@@ -1,0 +2,2 @@\n+added one\n+added two\n@@ -4,2 +5,0 @@\n-old four\n-old five";
        let expanded =
            expand_full_context(patch, "first\nadded one\nadded two\nsecond\nthird\nlast\n")
                .unwrap();
        insta::assert_snapshot!(expanded, @r"
        @@ -1,6 +1,6 @@
         first
        +added one
        +added two
         second
         third
        -old four
        -old five
         last
        ");
        for (target, expected) in [
            (SideLine::right(2), 2),
            (SideLine::right(4), 4),
            (SideLine::left(4), 6),
            (SideLine::left(5), 7),
            (SideLine::right(6), 8),
            (SideLine::left(6), 8),
        ] {
            assert_eq!(
                find_diff_line_index(&expanded, target),
                Some(expected),
                "{target:?}"
            );
        }
    }
}
