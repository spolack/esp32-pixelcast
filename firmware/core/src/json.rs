//! A small JSON writer for the web API: fields are added in order and the
//! commas, quotes and escaping are done here.
//!
//! ```
//! # use pixelcast_core::json;
//! let s = json::object(|o| {
//!     o.field("fps", 72).field("name", "a\"b").obj("wifi", |w| {
//!         w.field("on", false).field("ssid", None::<&str>);
//!     });
//! });
//! assert_eq!(s, r#"{"fps":72,"name":"a\"b","wifi":{"on":false,"ssid":null}}"#);
//! ```

use alloc::string::String;
use core::fmt::Write;

/// A value that can be written as JSON.
pub trait Value {
    fn write(&self, out: &mut String);
}

macro_rules! numbers {
    ($($t:ty),*) => {$(
        impl Value for $t {
            fn write(&self, out: &mut String) {
                let _ = write!(out, "{}", self);
            }
        }
    )*};
}
numbers!(u8, u16, u32, u64, usize, i8, i16, i32, i64);

impl Value for bool {
    fn write(&self, out: &mut String) {
        out.push_str(if *self { "true" } else { "false" });
    }
}

impl Value for str {
    fn write(&self, out: &mut String) {
        out.push('"');
        escape_into(self, out);
        out.push('"');
    }
}

impl Value for String {
    fn write(&self, out: &mut String) {
        self.as_str().write(out);
    }
}

impl<T: Value + ?Sized> Value for &T {
    fn write(&self, out: &mut String) {
        (**self).write(out);
    }
}

/// `None` is `null`.
impl<T: Value> Value for Option<T> {
    fn write(&self, out: &mut String) {
        match self {
            Some(v) => v.write(out),
            None => out.push_str("null"),
        }
    }
}

impl<T: Value> Value for [T] {
    fn write(&self, out: &mut String) {
        out.push('[');
        for (i, v) in self.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            v.write(out);
        }
        out.push(']');
    }
}

impl<T: Value, const N: usize> Value for [T; N] {
    fn write(&self, out: &mut String) {
        self.as_slice().write(out);
    }
}

/// Already formatted JSON, written as is.
pub struct Raw<'a>(pub &'a str);

impl Value for Raw<'_> {
    fn write(&self, out: &mut String) {
        out.push_str(self.0);
    }
}

/// Escapes `v` for a JSON string literal (without the quotes). `<` too, so
/// the text is safe inside HTML.
pub fn escape_into(v: &str, out: &mut String) {
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '<' => out.push_str("\\u003c"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

/// The fields of an object being written.
pub struct Obj<'a> {
    out: &'a mut String,
    first: bool,
}

impl Obj<'_> {
    fn key(&mut self, k: &str) {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
        k.write(self.out);
        self.out.push(':');
    }

    /// Adds `"k":v`.
    pub fn field(&mut self, k: &str, v: impl Value) -> &mut Self {
        self.key(k);
        v.write(self.out);
        self
    }

    /// Adds a nested object.
    pub fn obj(&mut self, k: &str, f: impl FnOnce(&mut Obj<'_>)) -> &mut Self {
        self.key(k);
        write_obj(self.out, f);
        self
    }

    /// Adds an array, filled by `f`.
    pub fn arr(&mut self, k: &str, f: impl FnOnce(&mut Arr<'_>)) -> &mut Self {
        self.key(k);
        write_arr(self.out, f);
        self
    }

    /// The output, for values written by hand (after [`Self::key_raw`]).
    pub fn key_raw(&mut self, k: &str) -> &mut String {
        self.key(k);
        self.out
    }
}

/// The items of an array being written.
pub struct Arr<'a> {
    out: &'a mut String,
    first: bool,
}

impl Arr<'_> {
    fn sep(&mut self) {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
    }

    pub fn item(&mut self, v: impl Value) -> &mut Self {
        self.sep();
        v.write(self.out);
        self
    }

    pub fn obj(&mut self, f: impl FnOnce(&mut Obj<'_>)) -> &mut Self {
        self.sep();
        write_obj(self.out, f);
        self
    }
}

fn write_obj(out: &mut String, f: impl FnOnce(&mut Obj<'_>)) {
    out.push('{');
    f(&mut Obj { out, first: true });
    out.push('}');
}

fn write_arr(out: &mut String, f: impl FnOnce(&mut Arr<'_>)) {
    out.push('[');
    f(&mut Arr { out, first: true });
    out.push(']');
}

/// Writes one object.
pub fn object(f: impl FnOnce(&mut Obj<'_>)) -> String {
    let mut out = String::new();
    write_obj(&mut out, f);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested() {
        let s = object(|o| {
            o.field("a", 1u8)
                .field("b", [1u16, 2, 3])
                .arr("c", |a| {
                    a.obj(|o| {
                        o.field("x", true);
                    })
                    .item("y")
                    .item(Raw("null"));
                })
                .obj("d", |_| {})
                .field("e", Some(-5i32))
                .field("f", None::<u32>);
        });
        assert_eq!(s, r#"{"a":1,"b":[1,2,3],"c":[{"x":true},"y",null],"d":{},"e":-5,"f":null}"#);
    }

    #[test]
    fn escapes() {
        let s = object(|o| {
            o.field("k\"", "a\\b\n</script>");
        });
        assert_eq!(s, r#"{"k\"":"a\\b\u000a\u003c/script>"}"#);
    }

    #[test]
    fn empty() {
        assert_eq!(object(|_| {}), "{}");
        assert_eq!(object(|o| { o.arr("a", |_| {}); }), r#"{"a":[]}"#);
    }
}
