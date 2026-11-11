This directory contains the Toasty SQLite driver from crates.io release 0.11.0,
commit `d71114e4101ce33ee5272ef2bd69ac6d5a6d749a`. The SHA-256 of the published
crate is `ffaebb64e79526666cb2628a7de5979da5ef282bb3f36a7e3b6a5acc55936794`.
The license comes from the same revision.

An empty workspace table keeps the driver outside the Rift workspace. The
manifest selects rusqlite 0.41.1 so the index and history store link the same
patched SQLite. Replace this directory with the published driver when its
rusqlite requirement accepts a release that supplies libsqlite3-sys 0.39 or later.
