use std::cell::RefCell;
use std::io::Write as _;
use std::path::PathBuf;

#[derive(Debug)]
pub(crate) struct AppContext {
    pub(crate) strict: bool,
    typetree_registries: Vec<PathBuf>,
    show_warnings: bool,
    warnings: RefCell<Vec<String>>,
}

impl AppContext {
    pub(crate) fn new(
        strict: bool,
        show_warnings: bool,
        typetree_registries: Vec<PathBuf>,
    ) -> Self {
        Self {
            strict,
            typetree_registries,
            show_warnings,
            warnings: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn typetree_registries(&self) -> &[PathBuf] {
        &self.typetree_registries
    }

    pub(crate) fn warn(&self, message: impl std::fmt::Display) {
        if self.show_warnings {
            self.warnings.borrow_mut().push(message.to_string());
        }
    }

    pub(crate) fn take_warnings(&self) -> Vec<String> {
        self.warnings.take()
    }

    pub(crate) fn flush_warnings(&self) {
        let stderr = std::io::stderr();
        let mut output = stderr.lock();
        for warning in self.take_warnings() {
            let _ = writeln!(output, "warning: {warning}");
        }
        let _ = output.flush();
    }
}
