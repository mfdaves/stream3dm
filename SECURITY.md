# Security

stream3dm reads files from untrusted sources, so a crash, a hang or unbounded memory on a
crafted `.3dm` or IFC file counts as a vulnerability, as does anything that reads or writes
outside the input it was given.

Please report it privately through GitHub's "Report a vulnerability" (Security tab of this
repository), with the file or a description of how to build it. You will get an answer within a
week; fixes are released as soon as they are ready and credited unless you prefer otherwise.

The reader reads one record at a time and checks lengths, counts and offsets before use; the
WebAssembly build runs inside the browser's sandbox and has no access to files or the network.
