//! What the Transcripts window does with the recordings: count how often
//! a search appears in one and find where, put them in order, narrow them to
//! one host, decide which day heading a recording belongs under, turn a
//! file's bytes into the text that is searched, break that text's very long
//! lines for the viewer ([`for_display`]), and map the viewer's offsets back to
//! the text, for copying ([`DisplayText::to_original`], [`char_slice`]).
//!
//! Plain Rust with no GTK types, so every rule here is covered by unit tests
//! that need no display. The window feeds it the listing from
//! [`moniker_core::transcript::list_recordings`] and the text of each
//! recording, and shows what comes back.

use std::ops::Range;

use moniker_core::transcript::Recording;
use time::Date;

/// How many times `query` appears in `text`: ignoring case, counting matches
/// that do not overlap (`aa` is in `aaaa` twice), and 0 for an empty `query`.
///
/// Case is folded one character at a time (`char::to_lowercase`), so `É`
/// matches `é`, and the result does not depend on the surrounding text (a
/// whole-string `to_lowercase` would give a capital sigma a different form at
/// the end of a word than in the middle of one).
pub fn count_matches(text: &str, query: &str) -> usize {
    let query = fold_case(query);
    if query.is_empty() {
        return 0;
    }
    fold_case(text).matches(query.as_str()).count()
}

/// `text` with every character in lower case.
fn fold_case(text: &str) -> String {
    text.chars().flat_map(char::to_lowercase).collect()
}

/// Which way the list is sorted by the time a session started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    NewestFirst,
    OldestFirst,
}

/// The recordings to show: only those from `host` (all of them for `None`),
/// sorted by start time in the given order. Recordings that started at the same
/// second are in file-name order either way, so the list does not shuffle
/// between refreshes.
pub fn arrange(recs: &[Recording], host: Option<&str>, order: Order) -> Vec<Recording> {
    let mut shown: Vec<Recording> = recs
        .iter()
        .filter(|r| host.is_none_or(|h| r.host == h))
        .cloned()
        .collect();
    shown.sort_by(|a, b| {
        let by_time = match order {
            Order::NewestFirst => b.started.cmp(&a.started),
            Order::OldestFirst => a.started.cmp(&b.started),
        };
        by_time.then_with(|| a.path.cmp(&b.path))
    });
    shown
}

/// The hosts that have recordings, each once, for the host filter: sorted
/// ignoring case (`Web` next to `web`, not after every lower-case name), and
/// by the exact text when two differ only in case.
pub fn hosts(recs: &[Recording]) -> Vec<String> {
    let mut hosts: Vec<String> = recs.iter().map(|r| r.host.clone()).collect();
    hosts.sort_by_cached_key(|h| (fold_case(h), h.clone()));
    hosts.dedup();
    hosts
}

/// The day heading a recording is listed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Day {
    Today,
    Yesterday,
    /// An earlier (or later) date as year, month (1 to 12) and day.
    Date(i32, u8, u8),
}

/// The day heading for a session that started on the local calendar date
/// `day`, when the local date is `today`.
///
/// Both dates are on the wall clock, not 24-hour spans: at 01:00 a session
/// from 23:00 the evening before is `Yesterday`, and one from 00:30 the same
/// morning is `Today`. A start date in the future (the clock changed) on
/// another day is a plain `Date`.
///
/// It takes dates, not instants and a time-zone offset, because the window
/// turns each instant into local time once (`glib::DateTime::from_unix_local`,
/// with the offset in force at that instant) and takes both the row's time of
/// day and this heading's date from that one conversion. So a row's time and
/// the heading above it always agree, across a daylight-saving change too,
/// which one "now" offset applied to every instant would not give.
pub fn day_of(day: Date, today: Date) -> Day {
    if day == today {
        Day::Today
    } else if today.previous_day() == Some(day) {
        Day::Yesterday
    } else {
        Day::Date(day.year(), u8::from(day.month()), day.day())
    }
}

/// Where `query` appears in `text`, as character ranges into `text` (the
/// offsets a `gtk::TextBuffer` uses), in order: the same matches as
/// [`count_matches`] counts (ignoring case, not overlapping), so the viewer's
/// "N of M" agrees with the list's "M matches". Empty for an empty `query`.
///
/// Matching runs on the case-folded text; each match is mapped back to the
/// characters of `text` it came from. A character whose lower case is longer
/// (`İ` folds to two) is covered whole when a match touches any part of it.
pub fn find_matches(text: &str, query: &str) -> Vec<Range<usize>> {
    let query = fold_case(query);
    if query.is_empty() {
        return Vec::new();
    }
    let folded = fold_case(text);
    // Walk `text` alongside the folded matches, which come in order: `index`
    // is the next character of `text` and `at` where its folding starts in
    // `folded`. No table from one to the other is kept, so a long recording
    // costs no extra memory here.
    let mut lens = text
        .chars()
        .map(|c| c.to_lowercase().map(char::len_utf8).sum::<usize>())
        .peekable();
    let (mut index, mut at) = (0usize, 0usize);
    // The character of `text` whose folding holds byte `byte` of `folded`.
    let mut char_at = |byte: usize| {
        while let Some(&len) = lens.peek() {
            if at + len > byte {
                break;
            }
            at += len;
            index += 1;
            lens.next();
        }
        index
    };
    folded
        .match_indices(query.as_str())
        .map(|(start, found)| {
            let first = char_at(start);
            let last = char_at(start + found.len() - 1);
            first..last + 1
        })
        .collect()
}

/// A recording's bytes as the text the window shows and searches: invalid
/// UTF-8 becomes U+FFFD (a transcript of a program that printed raw bytes, or
/// a file that is not a transcript at all), and so does a NUL, which a text
/// view cannot hold. One character stands for each, so [`find_matches`]
/// offsets into this text are offsets into what the viewer shows.
pub fn display_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace('\0', "\u{fffd}")
}

/// How many characters a line of a recording may have in the viewer before it
/// is broken. A `gtk::TextView` lays out one line as one piece of text, and
/// that gets very slow for a line of megabytes (a program that never printed a
/// newline): about 6 s for 5 MB. Broken into lines of this length it lays out
/// in a fraction of a second.
pub const DISPLAY_LINE_MAX: usize = 2000;

/// A recording's text as the viewer shows it: the same characters with a
/// newline inserted after every [`DISPLAY_LINE_MAX`] of them inside a longer
/// line, and the places where that was done.
///
/// Only the viewer's text view gets this text. The recording itself, Export,
/// and every search stay on the original text; [`to_display`](Self::to_display)
/// and [`range_to_display`](Self::range_to_display) turn an offset or a match
/// found there into where it is in this text, so the highlights land on the
/// right characters and a match that straddles a break is still one match,
/// highlighted across the newline. [`to_original`](Self::to_original) goes the
/// other way, so that copying a selection of this text copies the original's
/// characters, without the inserted newlines.
///
/// Offsets are in characters (Unicode scalar values), never bytes: the units
/// of [`find_matches`] and of a `gtk::TextBuffer`'s iterators.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DisplayText {
    /// The text to show.
    pub text: String,
    /// Where each inserted newline is, as the number of characters of the
    /// original text before it (so the offset of the first character after
    /// it), ascending.
    breaks: Vec<usize>,
}

/// What ends a line for a text view: a newline, a carriage return (alone or in
/// CRLF) or a paragraph separator, the paragraph boundaries `GtkTextBuffer`
/// splits its lines at. (A line separator, U+2028, does not end one.)
fn ends_line(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2029}')
}

/// `original` ready for the viewer: every line of more than
/// [`DISPLAY_LINE_MAX`] characters (its own terminator not counted) broken
/// after each [`DISPLAY_LINE_MAX`] characters. A break is only made before a
/// character that continues the line, never before its terminator, so a line
/// of exactly the maximum stays whole and no empty line appears. Text with no
/// long line comes back unchanged, with no breaks.
///
/// Breaks are made between characters, not at words or at the cells a
/// terminal would have used: this is a way to show an enormous line, not to
/// reflow text.
pub fn for_display(original: &str) -> DisplayText {
    let mut text = String::with_capacity(original.len());
    let mut breaks = Vec::new();
    // `original[..copied]` is in `text` already.
    let mut copied = 0;
    // The characters in `original[..counted]`, counted only as far as a long
    // line needs them, so text without one is not walked twice.
    let (mut counted, mut chars_before) = (0, 0);
    let mut line_start = 0;
    for line in original.split_inclusive(ends_line) {
        let start = line_start;
        line_start += line.len();
        // A line of at most this many bytes has at most as many characters.
        if line.len() <= DISPLAY_LINE_MAX {
            continue;
        }
        chars_before += original[counted..start].chars().count();
        counted = start;
        for (index, (at, c)) in line.char_indices().enumerate() {
            if index > 0 && index % DISPLAY_LINE_MAX == 0 && !ends_line(c) {
                text.push_str(&original[copied..start + at]);
                text.push('\n');
                copied = start + at;
                breaks.push(chars_before + index);
            }
        }
    }
    text.push_str(&original[copied..]);
    DisplayText { text, breaks }
}

impl DisplayText {
    /// Where the character at `original_char_offset` of the original text is
    /// in [`text`](Self::text): the offset plus the number of newlines
    /// inserted at or before it. The offset one past the last character maps
    /// to one past the last character of the display text.
    pub fn to_display(&self, original_char_offset: usize) -> usize {
        original_char_offset + self.breaks.partition_point(|&b| b <= original_char_offset)
    }

    /// Where a range of characters of the original text (a match from
    /// [`find_matches`]) is in [`text`](Self::text). It starts where its first
    /// character is and ends after where its last one is, so a range that ends
    /// where a break was inserted stops before the newline, while one that
    /// runs across a break includes it. An empty range stays empty.
    pub fn range_to_display(&self, range: &Range<usize>) -> Range<usize> {
        let start = self.to_display(range.start);
        let end = if range.is_empty() {
            start
        } else {
            self.to_display(range.end - 1) + 1
        };
        start..end
    }

    /// Where the character at `display_char_offset` of [`text`](Self::text)
    /// is in the original text: the inverse of [`to_display`](Self::to_display),
    /// so `to_original(to_display(o)) == o` for every offset `o` of the
    /// original, the one past its end included. An offset that falls on an
    /// inserted newline maps to its break's place in the original, the offset
    /// of the character after it (the newline stands for no character of the
    /// original). So a selection that starts or ends on a newline, or takes it
    /// in at its end, maps to the same original characters as one that leaves
    /// it out, and a selection across one maps to the characters on both sides
    /// with nothing between them.
    pub fn to_original(&self, display_char_offset: usize) -> usize {
        // The newline of break `k` (counting from 0) is at display offset
        // `breaks[k] + k`, and those offsets rise with `k`: count the newlines
        // before `display_char_offset` (a binary search, as a huge recording
        // has a break every 2,000 characters).
        let (mut low, mut high) = (0, self.breaks.len());
        while low < high {
            let mid = low + (high - low) / 2;
            if self.breaks[mid] + mid < display_char_offset {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        // Each newline counted is before the offset, so there are at most as
        // many as the offset itself.
        display_char_offset - low
    }
}

/// The characters `range` of `text`, counted in characters as a
/// [`DisplayText`] offset and a `gtk::TextBuffer` iterator are, as a slice of
/// `text`. A range that runs past the end stops there, and one that is empty or
/// backwards is the empty string.
pub fn char_slice(text: &str, range: Range<usize>) -> &str {
    // The byte offset of every character, then of the end.
    let mut at = text
        .char_indices()
        .map(|(byte, _)| byte)
        .chain(std::iter::once(text.len()));
    let start = at.nth(range.start).unwrap_or(text.len());
    // `nth(range.start)` took the first `range.start + 1` offsets.
    let end = match range.end.saturating_sub(range.start).checked_sub(1) {
        Some(after) => at.nth(after).unwrap_or(text.len()),
        None => start,
    };
    &text[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use time::Month;

    fn rec(name: &str, host: &str, started: i64) -> Recording {
        Recording {
            path: PathBuf::from("/t").join(name),
            host: host.to_owned(),
            started,
            bytes: 1,
        }
    }

    fn names(recs: &[Recording]) -> Vec<&str> {
        recs.iter()
            .map(|r| r.path.file_name().unwrap().to_str().unwrap())
            .collect()
    }

    #[test]
    fn counts_ignore_case_and_do_not_overlap() {
        assert_eq!(count_matches("Apt apt APT", "apt"), 3);
        assert_eq!(count_matches("aaaa", "aa"), 2);
        assert_eq!(count_matches("aaa", "aa"), 1);
        assert_eq!(count_matches("Apt apt APT", "APT"), 3);
        assert_eq!(count_matches("apt update\napt upgrade", "apt up"), 2);
        assert_eq!(count_matches("no match here", "apt"), 0);
        assert_eq!(count_matches("", "apt"), 0);
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        assert_eq!(count_matches("anything", ""), 0);
        assert_eq!(count_matches("", ""), 0);
    }

    #[test]
    fn a_query_is_text_not_a_pattern() {
        assert_eq!(count_matches("a.c abc a+c", "a.c"), 1);
        assert_eq!(count_matches("[x] [x]", "[x]"), 2);
        assert_eq!(count_matches("1 < 2 && 3 > 2", "&&"), 1);
    }

    #[test]
    fn case_is_folded_beyond_ascii() {
        assert_eq!(count_matches("Éé É", "é"), 3);
        assert_eq!(count_matches("ΑΣΑ ΑΣ", "ΑΣ"), 2);
    }

    #[test]
    fn arrange_filters_by_host_and_sorts_both_ways() {
        let recs = [
            rec("b", "web", 200),
            rec("a", "db", 300),
            rec("c", "web", 100),
            rec("d", "db", 100),
        ];
        let arranged = |host, order| names(&arrange(&recs, host, order)).join("");
        assert_eq!(arranged(None, Order::NewestFirst), "abcd");
        assert_eq!(arranged(None, Order::OldestFirst), "cdba");
        assert_eq!(arranged(Some("web"), Order::NewestFirst), "bc");
        assert_eq!(arranged(Some("web"), Order::OldestFirst), "cb");
        assert_eq!(arranged(Some("db"), Order::NewestFirst), "ad");
        assert_eq!(arranged(Some("nope"), Order::NewestFirst), "");
        assert_eq!(arrange(&[], None, Order::NewestFirst), []);
    }

    #[test]
    fn arrange_keeps_equal_times_in_name_order_both_ways() {
        let recs = [rec("z", "h", 5), rec("a", "h", 5), rec("m", "h", 5)];
        for order in [Order::NewestFirst, Order::OldestFirst] {
            assert_eq!(names(&arrange(&recs, None, order)), ["a", "m", "z"]);
        }
    }

    #[test]
    fn the_host_filter_is_exact() {
        let recs = [rec("a", "web", 1), rec("b", "Web", 2), rec("c", "web-2", 3)];
        assert_eq!(
            names(&arrange(&recs, Some("web"), Order::OldestFirst)),
            ["a"]
        );
    }

    #[test]
    fn arrange_does_not_change_its_input() {
        let recs = [rec("b", "web", 1), rec("a", "web", 2)];
        let before = recs.clone();
        arrange(&recs, Some("web"), Order::NewestFirst);
        assert_eq!(recs, before);
    }

    #[test]
    fn hosts_are_unique_and_sorted() {
        let recs = [
            rec("1", "web", 1),
            rec("2", "db", 2),
            rec("3", "web", 3),
            rec("4", "app", 4),
            rec("5", "db", 5),
        ];
        assert_eq!(hosts(&recs), ["app", "db", "web"]);
        assert!(hosts(&[]).is_empty());
    }

    #[test]
    fn hosts_sort_ignoring_case_but_stay_distinct() {
        let recs = [
            rec("1", "web", 1),
            rec("2", "Web", 2),
            rec("3", "Zed", 3),
            rec("4", "alpha", 4),
            rec("5", "web", 5),
        ];
        assert_eq!(hosts(&recs), ["alpha", "Web", "web", "Zed"]);
    }

    fn date(year: i32, month: u8, day: u8) -> Date {
        Date::from_calendar_date(year, Month::try_from(month).unwrap(), day).unwrap()
    }

    #[test]
    fn days_are_today_yesterday_or_a_date() {
        let today = date(2026, 10, 17);
        assert_eq!(day_of(date(2026, 10, 17), today), Day::Today);
        assert_eq!(day_of(date(2026, 10, 16), today), Day::Yesterday);
        assert_eq!(day_of(date(2026, 10, 15), today), Day::Date(2026, 10, 15));
        assert_eq!(day_of(date(2025, 10, 17), today), Day::Date(2025, 10, 17));
    }

    #[test]
    fn yesterday_crosses_month_and_year_ends() {
        let new_year = date(2027, 1, 1);
        assert_eq!(day_of(date(2026, 12, 31), new_year), Day::Yesterday);
        assert_eq!(
            day_of(date(2026, 12, 30), new_year),
            Day::Date(2026, 12, 30)
        );
        assert_eq!(day_of(date(2024, 2, 29), date(2024, 3, 1)), Day::Yesterday);
        assert_eq!(day_of(date(2026, 9, 30), date(2026, 10, 1)), Day::Yesterday);
    }

    #[test]
    fn a_start_date_in_the_future_is_a_date() {
        let today = date(2026, 10, 17);
        assert_eq!(day_of(date(2026, 10, 18), today), Day::Date(2026, 10, 18));
    }

    #[test]
    fn the_first_day_there_is_has_no_yesterday() {
        assert_eq!(day_of(Date::MIN, Date::MIN), Day::Today);
        let first = Day::Date(Date::MIN.year(), 1, 1);
        assert_eq!(day_of(Date::MIN, date(2026, 10, 17)), first);
    }

    /// The characters of `text` in each range.
    fn found(text: &str, query: &str) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        find_matches(text, query)
            .into_iter()
            .map(|r| chars[r].iter().collect())
            .collect()
    }

    #[test]
    fn matches_are_character_ranges_ignoring_case() {
        assert_eq!(find_matches("Apt apt APT", "apt"), [0..3, 4..7, 8..11]);
        assert_eq!(found("Apt apt APT", "apt"), ["Apt", "apt", "APT"]);
        assert_eq!(find_matches("aaaa", "aa"), [0..2, 2..4]);
        // Offsets count characters, not bytes.
        assert_eq!(find_matches("éé apt", "apt"), [Range { start: 3, end: 6 }]);
        assert_eq!(found("Éé É", "é"), ["É", "é", "É"]);
        assert!(find_matches("anything", "").is_empty());
        assert!(find_matches("", "x").is_empty());
        assert_eq!(found("1 < 2 && 3", "&&"), ["&&"]);
    }

    #[test]
    fn a_character_that_folds_longer_is_covered_whole() {
        // `İ` lower-cases to `i` and a combining dot: two characters.
        let text = "xİy İ";
        assert_eq!(found(text, "i"), ["İ", "İ"]);
        assert_eq!(found(text, "xi"), ["xİ"]);
        assert_eq!(find_matches(text, "y"), [Range { start: 2, end: 3 }]);
        assert_eq!(find_matches(text, "y").len(), count_matches(text, "y"));
    }

    #[test]
    fn matches_agree_with_the_count() {
        let texts = [
            "Apt apt APT",
            "aaaa aaa",
            "ΑΣΑ ΑΣ ας",
            "xİy İ i",
            "line one\nline two\r\nLINE",
            "",
        ];
        for text in texts {
            for query in ["a", "aa", "apt", "ας", "i", "line", "\n", "x"] {
                assert_eq!(
                    find_matches(text, query).len(),
                    count_matches(text, query),
                    "{text:?} / {query:?}"
                );
            }
        }
    }

    #[test]
    fn display_text_is_lossy_and_keeps_one_character_per_bad_byte() {
        assert_eq!(
            display_text(b"plain <b>&amp;</b>\n"),
            "plain <b>&amp;</b>\n"
        );
        assert_eq!(display_text(b"a\xffb"), "a\u{fffd}b");
        assert_eq!(display_text(b"a\0b"), "a\u{fffd}b");
        assert_eq!(display_text(b""), "");
    }

    // ----- display breaks -----

    /// `chars` of `text` in `range`, as a string.
    fn slice(text: &str, range: Range<usize>) -> String {
        text.chars().skip(range.start).take(range.len()).collect()
    }

    /// The display text with its inserted breaks taken out again.
    fn without_breaks(display: &DisplayText) -> String {
        let mut out = String::new();
        let mut inserted = display.breaks.iter().peekable();
        let mut original_index = 0;
        for (shown_index, c) in display.text.chars().enumerate() {
            // The inserted newline of the break at original offset `b` sits at
            // display offset `to_display(b) - 1`.
            if inserted
                .peek()
                .is_some_and(|&&b| display.to_display(b) - 1 == shown_index)
            {
                inserted.next();
                continue;
            }
            out.push(c);
            original_index += 1;
        }
        assert_eq!(out.chars().count(), original_index);
        out
    }

    #[test]
    fn short_lines_are_untouched() {
        for text in [
            "",
            "one line",
            "a\nb\r\nc\rd\n",
            "tail without a newline",
            "<b>&amp;</b>\n",
            "é\n\n\nñandú",
        ] {
            let display = for_display(text);
            assert_eq!(display.text, text);
            assert!(display.breaks.is_empty(), "{text:?}");
            assert_eq!(display.to_display(0), 0);
            assert_eq!(display.to_display(3), 3);
        }
        let exactly = "a".repeat(DISPLAY_LINE_MAX);
        assert_eq!(
            for_display(&exactly).text,
            exactly,
            "a full line stays whole"
        );
        let with_end = format!("{exactly}\n{exactly}\r\n{exactly}");
        assert_eq!(for_display(&with_end).text, with_end);
    }

    #[test]
    fn a_long_line_gets_breaks_every_max() {
        let original = "a".repeat(5_000);
        let display = for_display(&original);
        assert_eq!(display.breaks, [2_000, 4_000]);
        assert_eq!(display.text.chars().count(), 5_002);
        let lines: Vec<usize> = display
            .text
            .split('\n')
            .map(|l| l.chars().count())
            .collect();
        assert_eq!(lines, [2_000, 2_000, 1_000]);
        assert_eq!(without_breaks(&display), original);

        // One more character than a line holds is one break; exactly a line's
        // worth, or two, the line's end with none after it.
        let breaks = |n: usize| for_display(&"a".repeat(n)).breaks;
        assert!(breaks(DISPLAY_LINE_MAX).is_empty());
        assert_eq!(breaks(DISPLAY_LINE_MAX + 1), [DISPLAY_LINE_MAX]);
        assert_eq!(breaks(2 * DISPLAY_LINE_MAX), [DISPLAY_LINE_MAX]);
        assert_eq!(
            breaks(2 * DISPLAY_LINE_MAX + 1),
            [DISPLAY_LINE_MAX, 2 * DISPLAY_LINE_MAX]
        );
    }

    #[test]
    fn each_line_is_counted_from_its_own_start() {
        // Lines of 1,500 characters never add up to a break, whatever ends
        // them: a newline, a carriage return, a CRLF or a paragraph separator.
        let line = "b".repeat(1_500);
        for end in ["\n", "\r", "\r\n", "\u{2029}"] {
            let original = format!("{line}{end}{line}{end}{line}");
            let display = for_display(&original);
            assert!(display.breaks.is_empty(), "{end:?}");
            assert_eq!(display.text, original);
        }
        // A long line after short ones breaks at its own 2,000th character.
        let original = format!("ab\n{}\r\n{}", "c".repeat(2_500), "d".repeat(2_001));
        let display = for_display(&original);
        assert_eq!(display.breaks, [3 + 2_000, 3 + 2_500 + 2 + 2_000]);
        assert_eq!(without_breaks(&display), original);
        // A line's own terminator is not a character to break before: no
        // empty line is made after a line of exactly the maximum.
        let original = format!("{}\r\nx", "e".repeat(DISPLAY_LINE_MAX));
        assert!(for_display(&original).breaks.is_empty());
    }

    #[test]
    fn breaks_count_characters_not_bytes() {
        // 2,500 `é` are 5,000 bytes but fit two lines of characters.
        let original = "é".repeat(2_500);
        let display = for_display(&original);
        assert_eq!(display.breaks, [2_000]);
        let lines: Vec<usize> = display
            .text
            .split('\n')
            .map(|l| l.chars().count())
            .collect();
        assert_eq!(lines, [2_000, 500]);
        assert_eq!(without_breaks(&display), original);
        // 1,500 `é` are 3,000 bytes, more than a line's bytes, but one line.
        assert!(for_display(&"é".repeat(1_500)).breaks.is_empty());
        // Wide and multi-byte characters ahead of a long line move its offsets
        // by characters too: `ñ` (2 bytes) and `😀` (4 bytes) count once.
        let original = format!("ñ😀é\n{}", "z".repeat(2_001));
        let display = for_display(&original);
        assert_eq!(display.breaks, [4 + 2_000]);
        assert_eq!(without_breaks(&display), original);
    }

    #[test]
    fn offsets_map_across_breaks() {
        let display = for_display(&"a".repeat(5_000));
        // Before the first break nothing moves.
        assert_eq!(display.to_display(0), 0);
        assert_eq!(display.to_display(1_999), 1_999);
        // The character at a break's offset comes after the inserted newline.
        assert_eq!(display.to_display(2_000), 2_001);
        assert_eq!(display.to_display(3_999), 4_000);
        assert_eq!(display.to_display(4_000), 4_002);
        assert_eq!(display.to_display(5_000), 5_002, "the end of the text");
        // A range keeps the characters it covers and any newline between them,
        // but a range that ends where a line ends does not take the newline.
        assert_eq!(display.range_to_display(&(10..20)), 10..20);
        assert_eq!(display.range_to_display(&(1_998..2_004)), 1_998..2_005);
        assert_eq!(display.range_to_display(&(1_994..2_000)), 1_994..2_000);
        assert_eq!(display.range_to_display(&(2_000..2_006)), 2_001..2_007);
        assert_eq!(display.range_to_display(&(3_000..3_000)), 3_001..3_001);
        for range in [0..5_000, 1_999..2_001, 3_998..4_002] {
            let shown = display.range_to_display(&range);
            let covered = slice(&display.text, shown);
            assert_eq!(
                covered.replace('\n', "").chars().count(),
                range.len(),
                "{range:?}"
            );
        }
    }

    #[test]
    fn a_match_across_a_display_break_counts_once() {
        let mut original = "x".repeat(1_998);
        original.push_str("needle");
        original.push_str(&"y".repeat(3_000));
        let found = find_matches(&original, "needle");
        assert_eq!(found.len(), 1, "found once, in the original text");
        assert_eq!(found[0], 1_998..2_004);
        assert_eq!(count_matches(&original, "needle"), 1);

        let display = for_display(&original);
        assert_eq!(display.breaks, [2_000, 4_000]);
        let shown = display.range_to_display(&found[0]);
        assert_eq!(shown, 1_998..2_005, "one range across the inserted newline");
        assert_eq!(slice(&display.text, shown), "ne\nedle");
        // The display text is not searched: "needle" is not in it.
        assert_eq!(count_matches(&display.text, "needle"), 0);
    }

    #[test]
    fn display_offsets_map_back_to_the_original() {
        // Multi-byte characters ahead of and inside the long lines (`😀` is 4
        // bytes, `ß` and `ñ` 2), and a second long line after a short one.
        let mut original = String::from("é😀 ");
        original.push_str(&"ß".repeat(4_500));
        original.push_str("\nshort\n");
        original.push_str(&"ñ".repeat(2_001));
        let display = for_display(&original);
        assert_eq!(display.breaks.len(), 3);
        let chars = original.chars().count();
        let shown: Vec<char> = display.text.chars().collect();

        // Every offset of the original, the one past its end included, comes
        // back from where `to_display` put it.
        for offset in 0..=chars {
            assert_eq!(
                display.to_original(display.to_display(offset)),
                offset,
                "{offset}"
            );
        }
        // An inserted newline maps to its break's place in the original (the
        // character after it), as does the character after the newline; the
        // one before it is the character before the break.
        for &b in &display.breaks {
            let newline = display.to_display(b) - 1;
            assert_eq!(shown[newline], '\n', "{b}");
            assert_eq!(display.to_original(newline), b);
            assert_eq!(display.to_original(newline + 1), b);
            assert_eq!(display.to_original(newline - 1), b - 1);
        }
        // Every character shown that is not an inserted newline is the
        // character of the original it maps to.
        let chars_of_original: Vec<char> = original.chars().collect();
        for (at, &c) in shown.iter().enumerate() {
            let back = display.to_original(at);
            if display.to_display(back) == at {
                assert_eq!(chars_of_original[back], c, "{at}");
            }
        }
        assert_eq!(
            display.to_original(shown.len()),
            chars,
            "the end of the text"
        );
    }

    #[test]
    fn without_breaks_offsets_map_to_themselves() {
        let display = for_display("a\nb\r\nc");
        for offset in 0..=6 {
            assert_eq!(display.to_original(offset), offset);
        }
        assert_eq!(DisplayText::default().to_original(7), 7);
    }

    #[test]
    fn a_selection_across_a_break_is_the_original_text() {
        let mut original = "x".repeat(1_995);
        original.push_str("ÑANDÚ ñandú");
        original.push_str(&"é".repeat(3_000));
        let display = for_display(&original);
        assert_eq!(display.breaks, [2_000, 4_000]);
        // What a selection of the shown characters `from..to` copies.
        let copied = |from: usize, to: usize| {
            char_slice(
                &original,
                display.to_original(from)..display.to_original(to),
            )
            .to_owned()
        };
        // Shown: "NDÚ" ends the first display line, then the inserted newline
        // (at 2,000), then " ña". Selected across it, the newline is not there.
        assert_eq!(slice(&display.text, 1_997..2_004), "NDÚ\n ña");
        assert_eq!(copied(1_997, 2_004), "NDÚ ña");
        // Starting on the newline, or ending on it or just after it, neither
        // adds nor loses a character.
        assert_eq!(copied(2_000, 2_003), " ñ");
        assert_eq!(copied(1_997, 2_000), "NDÚ");
        assert_eq!(copied(1_997, 2_001), "NDÚ");
        assert_eq!(copied(2_000, 2_001), "", "only the newline: nothing");
        // Everything shown is everything in the file.
        assert_eq!(copied(0, display.text.chars().count()), original);
    }

    #[test]
    fn char_slices_count_characters() {
        let text = "é😀ñx";
        assert_eq!(char_slice(text, 1..3), "😀ñ");
        assert_eq!(char_slice(text, 0..4), text);
        assert_eq!(char_slice(text, 0..0), "");
        assert_eq!(char_slice(text, 4..4), "");
        assert_eq!(char_slice(text, 2..10), "ñx", "stops at the end");
        assert_eq!(char_slice(text, 9..12), "");
        let backwards = Range { start: 3, end: 1 };
        assert_eq!(
            char_slice(text, backwards),
            "",
            "a backwards range is empty"
        );
        assert_eq!(char_slice("", 0..1), "");
    }

    #[test]
    fn matches_map_by_characters_around_a_break() {
        // Multi-byte characters before and around the break: the match offsets
        // are characters of the original, so the mapped ones must be too.
        let mut original = String::from("é😀 ");
        original.push_str(&"ß".repeat(1_995));
        original.push_str("ÑANDÚ ñandú");
        original.push_str(&"é".repeat(2_500));
        let found = find_matches(&original, "ñandú");
        assert_eq!(found.len(), 2);
        let display = for_display(&original);
        let covered: Vec<String> = found
            .iter()
            .map(|r| slice(&display.text, display.range_to_display(r)))
            .collect();
        // 3 + 1,995 = 1,998 characters come first, so the first word is split
        // after its second character ("ÑA|NDÚ"); the second is whole.
        assert_eq!(covered, ["ÑA\nNDÚ", "ñandú"]);
        assert_eq!(without_breaks(&display), original);
    }
}
