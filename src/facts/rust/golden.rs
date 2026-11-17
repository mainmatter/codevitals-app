//! A file the extractor's facts are fixed for. Not compiled.
#![allow(dead_code)]

use std::fmt;

pub struct Point {
    x: i32,
    y: i32,
}

impl Point {
    pub fn new(x: i32, y: i32) -> Self {
        Point { x, y }
    }

    /// A method with a closure.
    fn scaled(&self, by: i32) -> Vec<i32> {
        [self.x, self.y].iter().map(|v| v * by).collect()
    }
}

impl fmt::Display for Point {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.x, self.y)
    }
}

trait Shape {
    fn area(&self) -> f64;

    fn describe(&self) -> String {
        if self.area() > 1.0 { "big".into() } else { "small".into() }
    }
}

fn classify(n: i32, flag: bool) -> Result<&'static str, String> {
    fn helper(n: i32) -> bool {
        n % 2 == 0 && n > 0 || n == -1
    }
    let Some(sign) = n.checked_abs() else {
        return Err("overflow".into());
    };
    let parse = |s: &str| -> Result<i32, String> { s.parse().map_err(|_| "bad".into()) };
    let _ = parse("1")?;
    for i in 0..sign {
        while flag {
            if i > 2 {
                break;
            } else if i > 1 {
                continue;
            }
        }
    }
    Ok(match n {
        0 => "zero",
        x if helper(x) => "even",
        _ => {
            let inner = || 1;
            if inner() == 1 { "odd" } else { "never" }
        }
    })
}

fn same_shape_as_helper(m: i32) -> bool {
    m % 3 == 1 && m > 7 || m == -9
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies() {
        let check = |n| classify(n, false).unwrap();
        assert_eq!(check(0), "zero");
    }

    fn fixture() -> Point {
        Point::new(1, 2)
    }
}

mod support {
    #![cfg(test)]

    pub fn always(_: u8) -> bool {
        true
    }
}

#[tokio::test]
async fn runs_async() {}

#[cfg(all(test, feature = "slow"))]
fn slow() {}

#[cfg(any(test, feature = "x"))]
fn sometimes() {}

macro_rules! unseen {
    () => {
        fn hidden() {}
    };
}

// Whether a `cfg` holds only with `test`.
#[cfg(/* reason */ test)]
fn commented() {}

#[cfg(all(unix, any(test, all(test, doc))))]
fn nested() {}

#[cfg(any(test, doc))]
fn either() {}

#[cfg(not(test))]
fn negated() {}

#[cfg(feature = "test")]
fn feature() {}

#[cfg(any())]
fn nothing() {}
