//! Compiles the QtNetwork shim against the system Qt (QMAKE is pinned in .cargo/config.toml).

fn main() {
    cxx_qt_build::CxxQtBuilder::new()
        .qt_module("Network")
        .cpp_file("src/qt_check.cpp")
        .build();
}
