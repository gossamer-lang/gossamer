# Standard library API guideline

The rules every `std` module follows, so a signature read in one module
predicts the shape of its neighbours. The mechanical ones are checked
over the signature table in CI (`signatures_follow_the_stdlib_api_guideline`
in `gossamer-types`).

## Bytes are `Vec<u8>`

Anything that is not text - a digest input, a key, a file body, a
compressed stream - is a `Vec<u8>` parameter named `data`, `bytes`,
`key`, `message`, or `contents`. A caller holding text passes
`s.as_bytes()`. A function whose input is text by nature (a pattern, a
path, a password, a template) takes a `String`.

When a module offers a text convenience beside the bytes form, the text
form carries a `_string` suffix (`crc32::checksum_string`) and answers
exactly what the bytes form answers for `s.as_bytes()`.

## Fixed-width values use their width

A checksum, a hash, or any value defined as N bits answers the unsigned
integer of that width: CRC-32, CRC-32C, Adler-32, and FNV-1a/32 answer
`u32`; FNV-1a/64 answers `u64`. A running value an `update` continues
takes the same type it answers. `i64` is for counts, sizes, and offsets.

## One error type per module

A module's fallible functions all answer the same error type. For the
filesystem that is `io::Error`; elsewhere it is `errors::Error`. The two
name one type, so `?` moves a value between them freely - the rule is
about what a signature says, so a reader never wonders whether two
siblings fail differently.

## A validated literal answers the value

A call the compiler checks while it builds the program -
`regex::compile("...")`, `sql::statement("...")` - answers the checked
value itself, never a `Result`: it cannot fail at run time, so a caller
should not have to handle a failure. The same operation over a value
built at run time has its own fallible name (`regex::new`) and answers a
`Result`.

## Documented types exist

A type, variant, or parameter a doc string names is one the signature
table declares. An option documented as a type (a compression level, a
mode) is either that type or documented as the integer it is.

## Changing a signature

A change to a shipped signature comes with a `gos fix` repair
(`crates/gossamer-lint/src/migrate.rs`) that restores every program the
change broke, and with a changelog line naming the rewriter.
