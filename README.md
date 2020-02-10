A command line utility for sending periodic email with the output of long
running commands.

[![Build status](https://api.travis-ci.org/BurntSushi/rust-cmail.png)](https://travis-ci.org/BurntSushi/rust-cmail)
[![](https://meritbadge.herokuapp.com/cmail)](https://crates.io/crates/cmail)

Dual-licensed under MIT or the [UNLICENSE](https://unlicense.org/).


### Installation

Currently, you have to build with Cargo, Rust's package manager. I'll hopefully
release binaries soon.

```
git clone git://github.com/BurntSushi/rust-cmail
cd rust-cmail
cargo build --release
./target/release/cmail --help
```

### Usage

```
Usage: cmail [options] [<args> ...]

Options:
    -h, --help             Display this help message.
    -p ARG, --period ARG   Data is emailed at this period in seconds.
                           Set to 0 to disable and send only one email
                           when the command completes.
                           [default: 900]
    -s, --silent           Don't pass the command's stdout/stderr to the
                           terminal. Instead, only send stdout/stderr
                           in email.
    -a, --send-all         Send the entire command's output on each email.
                           N.B. This saves all output in memory.
    -t ARG, --to ARG       The email address to send to. By default, this
                           is set to $EMAIL. If neither $EMAIL nor --to
                           are set, then an error is returned.
```

`cmail` responds gracefully to signals or if the command being run is
terminated unexpectedly. Internally, `cmail` uses `sendmail` to send email.


### Examples

Send whatever is on stdin:

```bash
cmail <<EOF
This is going to
my email
EOF
```

Report all 80 column violations in Lua files. Don't show output in terminal:

```bash
find ./ -name '*.lua' -print0 | xargs -0 colcheck | cmail -s
```

Run a `du` command on a huge directory, and send an incremental email update
every 10 seconds (the default is 15 minutes):

```bash
cmail -p 10 du -csh *
```


### Port from Go

This is ported from an earlier version I wrote in Go:
https://github.com/BurntSushi/cmail

Since this is my second run at writing this, it's a little cleaner this time
around. All in all, I'm quite fond of the structure of both programs (well,
the structure is quite similar!).
