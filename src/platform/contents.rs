use std::fmt;
use std::time::{Duration, Instant};

const TEXT_TYPES: [&str; 3] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];

/// Klipper stores nothing offered with this type set to `secret`.
const PASSWORD_MANAGER_HINT: &str = "x-kde-passwordManagerHint";
const SECRET: &[u8] = b"secret";

/// How much of a selection's previous contents is kept, over all its types.
const KEPT_LIMIT: usize = 64 * 1024 * 1024;
/// How long the application that holds a selection gets to hand one type over.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_millis(500);
const GRACE: Duration = Duration::from_millis(200);

const KEPT_TEXT_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];
const KEPT_RICH_TYPES: [&str; 4] = [
    "text/html",
    "text/uri-list",
    "x-special/gnome-copied-files",
    "application/x-kde-cutselection",
];
const PREFERRED_IMAGE_TYPE: &str = "image/png";

/// The final text as it is pasted, so that it cannot press Enter or send an escape sequence;
/// `None` when nothing but whitespace is left.
pub(crate) fn pasteable(text: &str) -> Option<String> {
    let mut pasted = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(char) = chars.next() {
        match char {
            '\r' => {
                chars.next_if_eq(&'\n');
                pasted.push(' ');
            }
            '\n' | '\u{85}' | '\u{2028}' | '\u{2029}' => pasted.push(' '),
            char if char.is_control() => {}
            char => pasted.push(char),
        }
    }
    (!pasted.trim().is_empty()).then_some(pasted)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) contents: Vec<(String, Vec<u8>)>,
}

impl Snapshot {
    pub(crate) fn data(&self, mime: &str) -> Option<&[u8]> {
        self.contents
            .iter()
            .find(|(kept, _)| kept == mime)
            .map(|(_, data)| data.as_slice())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.contents.is_empty()
    }
}

/// What a selection held before the final text replaced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Previous {
    Empty,
    /// Marked for password managers, so neither kept nor restored.
    Secret,
    Kept {
        snapshot: Snapshot,
        lost: Vec<(String, Lost)>,
    },
}

/// Why a type worth keeping is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Lost {
    TooBig,
    NoAnswer,
    Unreadable(String),
    NotAsked,
}

impl fmt::Display for Lost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooBig => write!(
                formatter,
                "it is larger than what is left of the {} MiB kept per selection",
                KEPT_LIMIT >> 20
            ),
            Self::NoAnswer => write!(
                formatter,
                "the application holding it did not hand it over within {} ms",
                READ_TIMEOUT.as_millis()
            ),
            Self::Unreadable(error) => write!(formatter, "it cannot be read: {error}"),
            Self::NotAsked => write!(
                formatter,
                "it was not asked for after an earlier type went unanswered"
            ),
        }
    }
}

/// Keeps the offered types worth keeping, reading each with what is left of [`KEPT_LIMIT`];
/// once one goes unanswered, the rest are not asked for, as each would wait as long.
pub(crate) fn keep(
    offered: &[String],
    mut read: impl FnMut(&str, usize) -> Result<Vec<u8>, Lost>,
) -> Previous {
    if offered.iter().any(|offer| offer == PASSWORD_MANAGER_HINT) {
        return Previous::Secret;
    }
    let mut left = KEPT_LIMIT;
    let mut contents = Vec::new();
    let mut lost = Vec::new();
    let mut answering = true;
    for mime in kept_types(offered) {
        let data = if answering {
            read(&mime, left).and_then(|data| {
                if data.len() <= left {
                    Ok(data)
                } else {
                    Err(Lost::TooBig)
                }
            })
        } else {
            Err(Lost::NotAsked)
        };
        match data {
            Ok(data) => {
                left -= data.len();
                contents.push((mime, data));
            }
            Err(why) => {
                answering &= why != Lost::NoAnswer;
                lost.push((mime, why));
            }
        }
    }
    Previous::Kept {
        snapshot: Snapshot { contents },
        lost,
    }
}

/// The offered types worth keeping, the small ones first; of its images only one type, PNG
/// where offered, since an application converts each image type it offers on request.
fn kept_types(offered: &[String]) -> Vec<String> {
    let offers = |kept: &str| offered.iter().any(|offer| offer == kept);
    let mut kept: Vec<String> = KEPT_TEXT_TYPES
        .iter()
        .chain(&KEPT_RICH_TYPES)
        .filter(|kept| offers(kept))
        .map(|kept| (*kept).to_owned())
        .collect();
    let image = if offers(PREFERRED_IMAGE_TYPE) {
        Some(PREFERRED_IMAGE_TYPE)
    } else {
        offered
            .iter()
            .map(String::as_str)
            .find(|offer| offer.starts_with("image/"))
    };
    kept.extend(image.map(str::to_owned));
    kept
}

pub(crate) fn final_text(text: &str) -> Snapshot {
    let mut contents: Vec<(String, Vec<u8>)> = TEXT_TYPES
        .iter()
        .map(|mime| ((*mime).to_owned(), text.as_bytes().to_vec()))
        .collect();
    contents.push((PASSWORD_MANAGER_HINT.to_owned(), SECRET.to_vec()));
    Snapshot { contents }
}

/// When the previous contents come back after the paste key was sent: [`GRACE`] after the
/// target's first read of the final text, at the latest after the timeout.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RestoreClock {
    latest: Instant,
    first_read: Option<Instant>,
}

impl RestoreClock {
    pub(crate) fn started(key_sent: Instant, timeout: Duration) -> Self {
        Self {
            latest: key_sent + timeout,
            first_read: None,
        }
    }

    /// Notes a read of the offered contents; only the final text's types count, since
    /// Klipper reads the password-manager hint of every new selection.
    pub(crate) fn read(&mut self, mime: &str, at: Instant) {
        if TEXT_TYPES.contains(&mime) && self.first_read.is_none() {
            self.first_read = Some(at);
        }
    }

    pub(crate) fn restore_at(&self) -> Instant {
        self.first_read
            .map_or(self.latest, |read| (read + GRACE).min(self.latest))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        GRACE, KEPT_LIMIT, Lost, Previous, RestoreClock, Snapshot, final_text, keep, kept_types,
        pasteable,
    };

    fn types(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn kept(contents: &[(&str, &[u8])], lost: &[(&str, Lost)]) -> Previous {
        Previous::Kept {
            snapshot: Snapshot {
                contents: contents
                    .iter()
                    .map(|(mime, data)| ((*mime).to_owned(), data.to_vec()))
                    .collect(),
            },
            lost: lost
                .iter()
                .map(|(mime, why)| ((*mime).to_owned(), why.clone()))
                .collect(),
        }
    }

    #[test]
    fn line_breaks_of_every_kind_become_one_space() {
        for text in [
            "a\nb",
            "a\r\nb",
            "a\rb",
            "a\u{85}b",
            "a\u{2028}b",
            "a\u{2029}b",
        ] {
            assert_eq!(pasteable(text).as_deref(), Some("a b"), "{text:?}");
        }
        assert_eq!(pasteable("a\n\nb").as_deref(), Some("a  b"));
        assert_eq!(pasteable("a\r\n").as_deref(), Some("a "));
    }

    #[test]
    fn every_other_control_character_is_dropped() {
        assert_eq!(
            pasteable("\u{1b}[201~rm\u{3}\u{4}\u{7f}\tx\u{9b}2~ Grüße").as_deref(),
            Some("[201~rmx2~ Grüße")
        );
        assert_eq!(pasteable("\0a\u{80}\u{9f}b").as_deref(), Some("ab"));
    }

    #[test]
    fn a_text_of_whitespace_and_control_characters_alone_is_not_pasted() {
        for text in ["", " ", "\n", " \r\n ", "\u{1b}", "\u{3}\u{2028}"] {
            assert_eq!(pasteable(text), None, "{text:?}");
        }
    }

    #[test]
    fn text_html_and_file_lists_are_kept_and_unknown_types_are_not() {
        let offered = types(&[
            "application/x-foo",
            "TEXT",
            "text/html",
            "text/plain;charset=utf-8",
            "text/uri-list",
            "STRING",
            "application/x-kde-cutselection",
            "x-special/gnome-copied-files",
            "UTF8_STRING",
            "chromium/x-web-custom-data",
        ]);

        assert_eq!(
            kept_types(&offered),
            types(&[
                "text/plain;charset=utf-8",
                "UTF8_STRING",
                "STRING",
                "TEXT",
                "text/html",
                "text/uri-list",
                "x-special/gnome-copied-files",
                "application/x-kde-cutselection",
            ])
        );
    }

    #[test]
    fn contents_marked_for_password_managers_are_neither_read_nor_kept() {
        let mut asked = Vec::new();

        let previous = keep(
            &types(&["text/plain", "x-kde-passwordManagerHint", "text/html"]),
            |mime, _| {
                asked.push(mime.to_owned());
                Ok(b"hunter2".to_vec())
            },
        );

        assert_eq!(previous, Previous::Secret);
        assert!(asked.is_empty(), "{asked:?}");
    }

    #[test]
    fn of_several_image_types_only_png_is_kept_and_last() {
        let offered = types(&["image/bmp", "image/png", "image/jpeg", "text/plain"]);

        assert_eq!(kept_types(&offered), types(&["text/plain", "image/png"]));
    }

    #[test]
    fn without_png_the_first_image_type_offered_is_kept() {
        let offered = types(&["image/jpeg", "image/bmp"]);

        assert_eq!(kept_types(&offered), types(&["image/jpeg"]));
    }

    #[test]
    fn nothing_offered_keeps_nothing() {
        assert_eq!(kept_types(&[]), Vec::<String>::new());
        assert_eq!(keep(&[], |_, _| unreachable!()), kept(&[], &[]));
    }

    #[test]
    fn each_type_is_read_with_what_is_left_of_the_limit_and_one_too_big_is_left_out() {
        let mut limits = Vec::new();
        let sizes = [
            ("text/plain", 10),
            ("text/html", KEPT_LIMIT),
            ("image/png", KEPT_LIMIT - 10),
        ];

        let previous = keep(
            &types(&["text/plain", "text/html", "image/png"]),
            |mime, limit| {
                limits.push((mime.to_owned(), limit));
                let size = sizes.iter().find(|(name, _)| *name == mime).unwrap().1;
                Ok(vec![b'x'; size])
            },
        );

        assert_eq!(
            limits,
            [
                ("text/plain".to_owned(), KEPT_LIMIT),
                ("text/html".to_owned(), KEPT_LIMIT - 10),
                ("image/png".to_owned(), KEPT_LIMIT - 10),
            ]
        );
        assert_eq!(
            previous,
            kept(
                &[
                    ("text/plain", &[b'x'; 10]),
                    ("image/png", &vec![b'x'; KEPT_LIMIT - 10]),
                ],
                &[("text/html", Lost::TooBig)],
            )
        );
    }

    #[test]
    fn a_type_that_cannot_be_read_is_left_out_and_named_with_why() {
        let previous = keep(&types(&["text/plain", "text/html"]), |mime, _| {
            if mime == "text/html" {
                Ok(b"<b>kept</b>".to_vec())
            } else {
                Err(Lost::Unreadable("broken pipe".to_owned()))
            }
        });

        assert_eq!(
            previous,
            kept(
                &[("text/html", b"<b>kept</b>")],
                &[("text/plain", Lost::Unreadable("broken pipe".to_owned()))],
            )
        );
    }

    #[test]
    fn once_a_type_goes_unanswered_the_rest_are_not_asked_for() {
        let mut asked = Vec::new();

        let previous = keep(
            &types(&["text/plain", "text/html", "text/uri-list"]),
            |mime, _| {
                asked.push(mime.to_owned());
                match mime {
                    "text/plain" => Ok(b"kept".to_vec()),
                    _ => Err(Lost::NoAnswer),
                }
            },
        );

        assert_eq!(asked, ["text/plain", "text/html"]);
        assert_eq!(
            previous,
            kept(
                &[("text/plain", b"kept")],
                &[
                    ("text/html", Lost::NoAnswer),
                    ("text/uri-list", Lost::NotAsked),
                ],
            )
        );
    }

    #[test]
    fn why_a_type_is_left_out_names_the_limit_and_the_wait() {
        assert_eq!(
            Lost::TooBig.to_string(),
            "it is larger than what is left of the 64 MiB kept per selection"
        );
        assert_eq!(
            Lost::NoAnswer.to_string(),
            "the application holding it did not hand it over within 500 ms"
        );
    }

    #[test]
    fn the_final_text_is_offered_as_text_and_marked_secret_for_klipper() {
        let text = "Grüße".as_bytes().to_vec();

        assert_eq!(
            final_text("Grüße"),
            Snapshot {
                contents: vec![
                    ("text/plain;charset=utf-8".to_owned(), text.clone()),
                    ("text/plain".to_owned(), text.clone()),
                    ("UTF8_STRING".to_owned(), text),
                    ("x-kde-passwordManagerHint".to_owned(), b"secret".to_vec()),
                ]
            }
        );
    }

    #[test]
    fn without_a_read_the_previous_contents_come_back_after_the_timeout() {
        let key_sent = Instant::now();
        let clock = RestoreClock::started(key_sent, Duration::from_secs(1));

        assert_eq!(clock.restore_at(), key_sent + Duration::from_secs(1));
    }

    #[test]
    fn the_first_read_of_the_text_brings_the_restore_forward_to_the_grace_after_it() {
        let key_sent = Instant::now();
        let mut clock = RestoreClock::started(key_sent, Duration::from_secs(1));

        clock.read(
            "text/plain;charset=utf-8",
            key_sent + Duration::from_millis(30),
        );
        clock.read("UTF8_STRING", key_sent + Duration::from_millis(500));

        assert_eq!(
            clock.restore_at(),
            key_sent + Duration::from_millis(30) + GRACE
        );
    }

    #[test]
    fn a_read_of_the_password_hint_does_not_count() {
        let key_sent = Instant::now();
        let mut clock = RestoreClock::started(key_sent, Duration::from_secs(1));

        clock.read(
            "x-kde-passwordManagerHint",
            key_sent + Duration::from_millis(10),
        );

        assert_eq!(clock.restore_at(), key_sent + Duration::from_secs(1));
    }

    #[test]
    fn a_read_shortly_before_the_timeout_does_not_push_the_restore_past_it() {
        let key_sent = Instant::now();
        let mut clock = RestoreClock::started(key_sent, Duration::from_millis(100));

        clock.read("text/plain", key_sent + Duration::from_millis(90));

        assert_eq!(clock.restore_at(), key_sent + Duration::from_millis(100));
    }
}
