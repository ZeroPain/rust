// Copyright 2018 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// edition:2018

enum Foo {}

struct std;

fn main() {
    enum Foo { A, B }
    use Foo::*;
    //~^ ERROR `Foo` is ambiguous

    let _ = (A, B);

    fn std() {}
    enum std {}
    use std as foo;
    //~^ ERROR `std` is ambiguous
    //~| ERROR `std` is ambiguous
}
