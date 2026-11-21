fn main() -> iced::Result {
    // Logging installs inside `pige::run`, shared with the Android entry
    // point; `RUST_LOG=info ./run.sh` surfaces provider and auth warnings.
    pige::run()
}
