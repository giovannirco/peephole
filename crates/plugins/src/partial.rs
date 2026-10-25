use peephole_core::Value;

/// Collects the successes of a poll made of independent parts (queries, fields,
/// endpoints): failures are logged and counted; only all-failed is an error.
pub(crate) struct Partial {
    plugin: String,
    counter: &'static str,
    values: Vec<(String, Value)>,
    ok: usize,
    failed: Vec<String>,
}

impl Partial {
    pub(crate) fn new(plugin: &str, counter: &'static str) -> Self {
        Self { plugin: plugin.to_owned(), counter, values: Vec::new(), ok: 0, failed: Vec::new() }
    }

    pub(crate) fn ok(&mut self, values: impl IntoIterator<Item = (String, Value)>) {
        self.ok += 1;
        self.values.extend(values);
    }

    pub(crate) fn fail(&mut self, part: &str, error: &anyhow::Error) {
        tracing::warn!(plugin = %self.plugin, part, error = format!("{error:#}"), "part of the poll failed");
        self.failed.push(format!("{part}: {error:#}"));
    }

    pub(crate) fn finish(mut self) -> anyhow::Result<Vec<(String, Value)>> {
        if self.ok == 0 && !self.failed.is_empty() {
            anyhow::bail!("every one of {} {} failed: {}", self.failed.len(), self.counter, self.failed.join("; "));
        }
        self.values.push((format!("_{}_ok", self.counter), Value::Number(self.ok as f64)));
        self.values.push((format!("_{}_failed", self.counter), Value::Number(self.failed.len() as f64)));
        Ok(self.values)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn keeps_successes_and_counts_both_sides() {
        let mut p = Partial::new("prom", "queries");
        p.ok([("a".into(), Value::Number(1.0))]);
        p.fail("b", &anyhow::anyhow!("boom"));
        let got: BTreeMap<_, _> = p.finish().unwrap().into_iter().collect();
        assert_eq!(got["a"], Value::Number(1.0));
        assert_eq!(got["_queries_ok"], Value::Number(1.0));
        assert_eq!(got["_queries_failed"], Value::Number(1.0));
    }

    #[test]
    fn all_failed_is_an_error_naming_each_part() {
        let mut p = Partial::new("prom", "queries");
        p.fail("cpu", &anyhow::anyhow!("timeout"));
        p.fail("mem", &anyhow::anyhow!("bad expr"));
        let err = p.finish().unwrap_err().to_string();
        assert!(err.contains("cpu: timeout") && err.contains("mem: bad expr"), "{err}");
    }

    #[test]
    fn nothing_attempted_is_not_an_error() {
        let got = Partial::new("p", "fields").finish().unwrap();
        assert_eq!(got.len(), 2);
    }
}
