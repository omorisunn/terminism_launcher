use std::cell::RefCell;
use std::collections::VecDeque;
use std::ops::Deref;
use std::rc::Rc;

pub type LogStore = Rc<RefCell<LogBuffer>>;
pub const LOG_CAP: usize = 1000;

#[derive(Debug, Default)]
pub struct LogBuffer {
    lines: VecDeque<String>,
    revision: u64,
}

impl LogBuffer {
    pub fn push(&mut self, line: String) {
        self.lines.push_back(line);
        if self.lines.len() > LOG_CAP {
            self.lines.pop_front();
        }
        self.revision += 1;
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn join(&self, separator: &str) -> String {
        self.lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(separator)
    }
}

impl Deref for LogBuffer {
    type Target = VecDeque<String>;
    fn deref(&self) -> &Self::Target {
        &self.lines
    }
}

#[derive(Clone)]
pub struct Logger {
    pub tag: String,
    sink: LogStore,
}

impl Logger {
    pub fn new(tag: &str, sink: LogStore) -> Logger {
        Logger {
            tag: tag.to_string(),
            sink,
        }
    }

    pub fn child(&self, id: &str) -> Logger {
        Logger::new(&format!("{}.{}", self.tag, id), self.sink.clone())
    }

    pub fn log(&self, text: &str) {
        self.sink
            .borrow_mut()
            .push(format!("[{}] {}", self.tag, text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logger_chain_tags() {
        let sink = LogStore::default();
        let root = Logger::new("a", sink.clone());
        let child = root.child("b");
        let grandchild = child.child("c");
        root.log("one");
        child.log("two");
        grandchild.log("three");
        assert_eq!(
            sink.borrow().join("\n"),
            "[a] one\n[a.b] two\n[a.b.c] three"
        );
    }

    #[test]
    fn bounded_logs_keep_a_monotonic_revision() {
        let sink = LogStore::default();
        let logger = Logger::new("same", sink.clone());
        for _ in 0..LOG_CAP + 5 {
            logger.log("repeated");
        }
        assert_eq!(sink.borrow().len(), LOG_CAP);
        assert_eq!(sink.borrow().revision(), (LOG_CAP + 5) as u64);
    }
}
