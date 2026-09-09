//! Zerocopy IRC message parser.
//!
//! `Message` borrows byte-slices directly out of the source buffer (the
//! reassembled TCP stream) — no allocation, no copy. Parsing follows the
//! RFC 1459 / 2812 grammar:
//!
//! ```text
//! [ ':' prefix SPACE ] command { SPACE param } [ SPACE ':' trailing ]
//! ```
//!
//! The trailing parameter (introduced by `:`, may contain spaces) is stored as
//! the final element of `params`.

/// RFC 2812 caps a message at 15 parameters.
const MAX_PARAMS: usize = 15;

#[derive(Clone)]
pub struct Message<'a> {
    prefix: Option<&'a [u8]>,
    command: &'a [u8],
    params: [&'a [u8]; MAX_PARAMS],
    nparams: usize,
}

impl<'a> Message<'a> {
    /// Parse one line (CR/LF already stripped). Returns `None` if there is no
    /// command. All returned slices point into `line`.
    pub fn parse(line: &'a [u8]) -> Option<Message<'a>> {
        let mut rest = line;

        let prefix = if rest.first() == Some(&b':') {
            let end = rest.iter().position(|&b| b == b' ')?;
            let p = &rest[1..end];
            rest = skip_spaces(&rest[end..]);
            Some(p)
        } else {
            None
        };

        let cmd_end = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
        let command = &rest[..cmd_end];
        if command.is_empty() {
            return None;
        }
        rest = &rest[cmd_end..];

        let mut params: [&'a [u8]; MAX_PARAMS] = [&line[0..0]; MAX_PARAMS];
        let mut nparams = 0;
        loop {
            rest = skip_spaces(rest);
            if rest.is_empty() {
                break;
            }
            if rest[0] == b':' {
                // Trailing parameter: everything after the ':' verbatim.
                if nparams < MAX_PARAMS {
                    params[nparams] = &rest[1..];
                    nparams += 1;
                }
                break;
            }
            let end = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
            if nparams < MAX_PARAMS {
                params[nparams] = &rest[..end];
                nparams += 1;
            }
            rest = &rest[end..];
        }

        Some(Message {
            prefix,
            command,
            params,
            nparams,
        })
    }

    pub fn prefix(&self) -> Option<&'a [u8]> {
        self.prefix
    }

    pub fn command(&self) -> &'a [u8] {
        self.command
    }

    pub fn params(&self) -> &[&'a [u8]] {
        &self.params[..self.nparams]
    }

    /// Case-insensitive command match (e.g. `msg.is("PING")`).
    pub fn is(&self, name: &[u8]) -> bool {
        self.command.eq_ignore_ascii_case(name)
    }

    /// The last parameter, which is the trailing one when present.
    pub fn last_param(&self) -> Option<&'a [u8]> {
        self.nparams.checked_sub(1).map(|i| self.params[i])
    }
}

fn skip_spaces(mut s: &[u8]) -> &[u8] {
    while s.first() == Some(&b' ') {
        s = &s[1..];
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_privmsg_with_prefix_and_trailing() {
        let line = b":nick!user@host PRIVMSG #chan :hello world";
        let m = Message::parse(line).unwrap();
        assert_eq!(m.prefix(), Some(&b"nick!user@host"[..]));
        assert_eq!(m.command(), b"PRIVMSG");
        assert_eq!(m.params(), &[&b"#chan"[..], &b"hello world"[..]]);
        assert_eq!(m.last_param(), Some(&b"hello world"[..]));
    }

    #[test]
    fn parses_ping_without_prefix() {
        let m = Message::parse(b"PING :token123").unwrap();
        assert_eq!(m.prefix(), None);
        assert!(m.is(b"ping")); // case-insensitive
        assert_eq!(m.last_param(), Some(&b"token123"[..]));
    }

    #[test]
    fn parses_numeric_reply() {
        let m = Message::parse(b":irc.test 001 tester :Welcome to IRC").unwrap();
        assert_eq!(m.command(), b"001");
        assert_eq!(m.params()[0], b"tester");
        assert_eq!(m.params()[1], b"Welcome to IRC");
    }

    #[test]
    fn command_only_no_params() {
        let m = Message::parse(b"PING").unwrap();
        assert_eq!(m.command(), b"PING");
        assert!(m.params().is_empty());
        assert_eq!(m.last_param(), None);
    }

    #[test]
    fn tolerates_extra_spaces() {
        let m = Message::parse(b":s  JOIN   #chan").unwrap();
        assert_eq!(m.prefix(), Some(&b"s"[..]));
        assert_eq!(m.command(), b"JOIN");
        assert_eq!(m.params(), &[&b"#chan"[..]]);
    }

    #[test]
    fn empty_or_prefix_only_is_none() {
        assert!(Message::parse(b"").is_none());
        assert!(Message::parse(b":only-prefix").is_none()); // no command after prefix
    }

    #[test]
    fn colon_in_trailing_is_preserved() {
        let m = Message::parse(b"PRIVMSG #c :a:b:c http://x").unwrap();
        assert_eq!(m.last_param(), Some(&b"a:b:c http://x"[..]));
    }

    #[test]
    fn fields_borrow_from_source_buffer() {
        // The whole point: every field is a slice into `line`, not a copy.
        let line = b":srv 353 me = #c :alice bob";
        let m = Message::parse(line).unwrap();
        let base = line.as_ptr() as usize;
        let end = base + line.len();
        for slice in std::iter::once(m.command())
            .chain(m.prefix())
            .chain(m.params().iter().copied())
        {
            if slice.is_empty() {
                continue;
            }
            let p = slice.as_ptr() as usize;
            assert!(p >= base && p < end, "field must borrow from source line");
        }
    }
}
