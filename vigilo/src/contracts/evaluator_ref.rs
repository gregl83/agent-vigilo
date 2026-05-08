#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EvaluatorIdentity {
    pub(crate) namespace: String,
    pub(crate) name: String,
    pub(crate) version: String,
}

pub(crate) fn parse_fully_qualified_evaluator(input: &str) -> anyhow::Result<EvaluatorIdentity> {
    let (identity, version) = input
        .rsplit_once(':')
        .map(|(l, r)| (l.trim(), r.trim()))
        .ok_or_else(|| anyhow::anyhow!(
            "ambiguous evaluator identifier '{}'; use fully qualified '<namespace>/<name>:<version>'",
            input
        ))?;

    let (namespace, name) = identity
        .rsplit_once('/')
        .map(|(l, r)| (l.trim(), r.trim()))
        .ok_or_else(|| anyhow::anyhow!(
            "ambiguous evaluator identifier '{}'; use fully qualified '<namespace>/<name>:<version>'",
            input
        ))?;

    if namespace.is_empty() || name.is_empty() || version.is_empty() {
        anyhow::bail!(
            "ambiguous evaluator identifier '{}'; use fully qualified '<namespace>/<name>:<version>'",
            input
        );
    }

    Ok(EvaluatorIdentity {
        namespace: namespace.to_string(),
        name: name.to_string(),
        version: version.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_fully_qualified_evaluator;

    #[test]
    fn parse_fully_qualified_evaluator_accepts_new_format() {
        let parsed = parse_fully_qualified_evaluator("vigilo/sentiment-basic-en:0.1.0").unwrap();
        assert_eq!(parsed.namespace, "vigilo");
        assert_eq!(parsed.name, "sentiment-basic-en");
        assert_eq!(parsed.version, "0.1.0");
    }

    #[test]
    fn parse_fully_qualified_evaluator_rejects_old_format() {
        let err = parse_fully_qualified_evaluator("vigilo:sentiment-basic-en@0.1.0").unwrap_err();
        assert!(
            err.to_string().contains("<namespace>/<name>:<version>"),
            "unexpected error message: {}",
            err
        );
    }
}
