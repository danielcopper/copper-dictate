use std::time::{Duration, Instant};

pub(crate) const TEXT_TYPES: [&str; 3] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];

/// Klipper stores nothing offered with this type set to [`SECRET`].
pub(crate) const PASSWORD_MANAGER_HINT: &str = "x-kde-passwordManagerHint";
pub(crate) const SECRET: &[u8] = b"secret";

/// How much of the previous contents is kept, over all its types.
pub(crate) const KEPT_LIMIT: usize = 64 * 1024 * 1024;

/// How long the target gets after its first read of the final text before the previous
/// contents come back.
pub(crate) const GRACE: Duration = Duration::from_millis(200);

const KEPT_TEXT_TYPES: [&str; 6] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
    PASSWORD_MANAGER_HINT,
];
const KEPT_RICH_TYPES: [&str; 4] = [
    "text/html",
    "text/uri-list",
    "x-special/gnome-copied-files",
    "application/x-kde-cutselection",
];
const PREFERRED_IMAGE_TYPE: &str = "image/png";

/// The previous contents' types worth keeping, the small ones first; of its images only one
/// type, PNG where offered, since an application converts each image type it offers on request.
pub(crate) fn kept_types(offered: &[String]) -> Vec<String> {
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) contents: Vec<(String, Vec<u8>)>,
}

impl Snapshot {
    /// Reads each type in order, giving each read what is left of [`KEPT_LIMIT`]; a type that
    /// does not fit or cannot be read is left out.
    pub(crate) fn take(
        types: &[String],
        mut read: impl FnMut(&str, usize) -> Option<Vec<u8>>,
    ) -> Self {
        let mut left = KEPT_LIMIT;
        let mut contents = Vec::new();
        for mime in types {
            if let Some(data) = read(mime, left).filter(|data| data.len() <= left) {
                left -= data.len();
                contents.push((mime.clone(), data));
            }
        }
        Self { contents }
    }

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
        GRACE, KEPT_LIMIT, PASSWORD_MANAGER_HINT, RestoreClock, SECRET, Snapshot, TEXT_TYPES,
        final_text, kept_types,
    };

    fn types(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn text_html_file_lists_and_the_password_hint_are_kept_and_unknown_types_are_not() {
        let offered = types(&[
            "application/x-foo",
            "text/html",
            "text/plain;charset=utf-8",
            "x-kde-passwordManagerHint",
            "text/uri-list",
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
                "x-kde-passwordManagerHint",
                "text/html",
                "text/uri-list",
                "x-special/gnome-copied-files",
                "application/x-kde-cutselection",
            ])
        );
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
    }

    #[test]
    fn each_type_is_read_with_what_is_left_of_the_limit_and_one_too_big_is_left_out() {
        let mut limits = Vec::new();
        let sizes = [
            ("text/plain", 10),
            ("text/html", KEPT_LIMIT),
            ("image/png", KEPT_LIMIT - 10),
        ];

        let snapshot = Snapshot::take(
            &types(&["text/plain", "text/html", "image/png"]),
            |mime, limit| {
                limits.push((mime.to_owned(), limit));
                let size = sizes.iter().find(|(name, _)| *name == mime).unwrap().1;
                Some(vec![b'x'; size])
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
            snapshot,
            Snapshot {
                contents: vec![
                    ("text/plain".to_owned(), vec![b'x'; 10]),
                    ("image/png".to_owned(), vec![b'x'; KEPT_LIMIT - 10]),
                ]
            }
        );
    }

    #[test]
    fn a_type_that_cannot_be_read_is_left_out() {
        let snapshot = Snapshot::take(&types(&["text/plain", "text/html"]), |mime, _| {
            (mime == "text/html").then(|| b"<b>kept</b>".to_vec())
        });

        assert_eq!(snapshot.data("text/plain"), None);
        assert_eq!(snapshot.data("text/html"), Some(&b"<b>kept</b>"[..]));
    }

    #[test]
    fn the_final_text_is_offered_as_text_and_marked_secret_for_klipper() {
        let offered = final_text("Grüße");

        for mime in TEXT_TYPES {
            assert_eq!(offered.data(mime), Some("Grüße".as_bytes()), "{mime}");
        }
        assert_eq!(offered.data(PASSWORD_MANAGER_HINT), Some(SECRET));
        assert_eq!(SECRET, b"secret");
        assert_eq!(offered.contents.len(), TEXT_TYPES.len() + 1);
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

        clock.read(PASSWORD_MANAGER_HINT, key_sent + Duration::from_millis(10));

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
