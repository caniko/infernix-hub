use domain::ports::{HumanizeInput, HumanizeOutput, HumanizeRequest, HumanizeResult, Humanizer, PortError, PortResult};

pub struct FixtureHumanizer;

#[async_trait::async_trait]
impl Humanizer for FixtureHumanizer {
    async fn humanize(&self, req: HumanizeInput) -> PortResult<HumanizeOutput> {
        Ok(rewrite(&req.text))
    }

    async fn humanize_with_voice(&self, req: &HumanizeRequest) -> PortResult<HumanizeResult> {
        Ok(rewrite(&req.text))
    }
}

fn rewrite(text: &str) -> HumanizeOutput {
    HumanizeOutput {
        humanized_text: format!("Fixture rewrite: {text}"),
        paragraphs_processed: text.split("\n\n").filter(|p| !p.trim().is_empty()).count(),
        retries_used: 0,
        fallback_paragraphs: Vec::new(),
        judge_verified: true,
        paragraphs: None,
    }
}

pub struct DisabledHumanizer;

#[async_trait::async_trait]
impl Humanizer for DisabledHumanizer {
    async fn humanize(&self, _: HumanizeInput) -> PortResult<HumanizeOutput> {
        Err(PortError::Unsupported("humanizer integration is disabled".into()))
    }

    fn is_enabled(&self) -> bool {
        false
    }
}
