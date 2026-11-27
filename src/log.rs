use std::fmt::Display;
use std::io::{self, Write};

const PREFIX: &str = "copper-dictate: ";

pub(crate) fn eprint(message: impl Display) {
    eprintln!("{PREFIX}{message}");
}

pub(crate) fn write(out: &mut dyn Write, message: impl Display) -> io::Result<()> {
    writeln!(out, "{PREFIX}{message}")
}

#[cfg(test)]
mod tests {
    use super::{PREFIX, write};
    use crate::test_support::production_code;

    #[test]
    fn a_line_gets_the_prefix() {
        let mut out = Vec::new();

        write(&mut out, format_args!("{} words", 2)).unwrap();

        assert_eq!(out, b"copper-dictate: 2 words\n");
    }

    #[test]
    fn no_production_code_spells_out_the_prefix() {
        let spelled_out: Vec<String> = production_code()
            .into_iter()
            .filter(|(name, code)| name != "log.rs" && code.contains(PREFIX))
            .map(|(name, _)| name)
            .collect();

        assert!(spelled_out.is_empty(), "{spelled_out:?}");
    }
}
