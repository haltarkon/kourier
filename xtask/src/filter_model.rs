//! A model of the nextest filter language for tests of xtask (compiled only in tests): the
//! filters `ci` builds are judged by evaluating them over a made-up workspace, not by comparing
//! strings. It knows what ci's filters use: `kind(test)`, `package(=x)`, `test(=name)`, `&`,
//! `|`, `!` and parentheses; any other predicate panics, so a filter that grows a new kind of
//! predicate fails its test until the model learns it. `scripts/nextest-check.py` runs the real
//! nextest over the real workspace for the same claims.

#[derive(Debug, Clone)]
pub struct Item {
    pub package: &'static str,
    pub name: &'static str,
    pub kind: &'static str,
}

struct Parser<'a> {
    text: &'a [u8],
    at: usize,
    item: &'a Item,
}

impl Parser<'_> {
    fn skip(&mut self) {
        while self.text.get(self.at) == Some(&b' ') {
            self.at += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip();
        self.text.get(self.at).copied()
    }

    fn or(&mut self) -> bool {
        let mut value = self.and();
        while self.peek() == Some(b'|') {
            self.at += 1;
            let rhs = self.and();
            value = value || rhs;
        }
        value
    }

    fn and(&mut self) -> bool {
        let mut value = self.unary();
        while self.peek() == Some(b'&') {
            self.at += 1;
            let rhs = self.unary();
            value = value && rhs;
        }
        value
    }

    fn unary(&mut self) -> bool {
        match self.peek() {
            Some(b'!') => {
                self.at += 1;
                !self.unary()
            }
            Some(b'(') => {
                self.at += 1;
                let value = self.or();
                assert_eq!(self.peek(), Some(b')'), "a closing parenthesis");
                self.at += 1;
                value
            }
            _ => self.predicate(),
        }
    }

    fn predicate(&mut self) -> bool {
        let start = self.at;
        while self.text[self.at] != b'(' {
            self.at += 1;
        }
        let name = std::str::from_utf8(&self.text[start..self.at])
            .unwrap()
            .trim();
        self.at += 1;
        let arg_start = self.at;
        while self.text[self.at] != b')' {
            self.at += 1;
        }
        let arg = std::str::from_utf8(&self.text[arg_start..self.at]).unwrap();
        self.at += 1;
        match (name, arg.strip_prefix('=')) {
            ("kind", None) => self.item.kind == arg,
            ("package", Some(exact)) => self.item.package == exact,
            ("test", Some(exact)) => self.item.name == exact,
            other => panic!("a predicate the model does not know: {other:?}"),
        }
    }
}

pub fn matches(filter: &str, item: &Item) -> bool {
    let mut parser = Parser {
        text: filter.as_bytes(),
        at: 0,
        item,
    };
    let value = parser.or();
    assert_eq!(parser.peek(), None, "the whole filter was read: {filter}");
    value
}
