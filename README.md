# MAVLink Extras

A package with some utilities for using MAVLink asynchronously.

## Why you might want to use this crate

Currently there are a few alternatives of crates for connecting to MAVLink, namely the
[`mavlink`](https://crates.io/crates/mavlink) crate itself and
[`async-mavlink`](https://crates.io/crates/async-mavlink). However both of these crate have their
limitations.

- [`mavlink`](https://crates.io/crates/mavlink)
    - No async
- [`async-mavlink`](https://crates.io/crates/async-mavlink)
    - Uses extra thread for blocking under the hood.
    - Uses unbounded message queues that may grow indefinitely in case some subscriber takes too
      long to receive a message. It is a problem when receiving hundreds of messages per second
      where many messages can be discarded.

This crate offers solutions to the above problems by providing full async compatibility through
[`tokio_util::codec`](https://docs.rs/tokio-util/0.7.7/tokio_util/codec/index.html) and a mailbox
structure that stores only the most common messages in memory. The entire connection can run off a
single CPU thread, since it leverages of nonblocking IO.

## Limitations

- Currently only supports the [`tokio`](https://crates.io/crates/tokio) runtime.
- Currently missing full support for message types other than [`common::MavMessage`](https://docs.rs/mavlink/0.10.1/mavlink/common/enum.MavMessage.html).
