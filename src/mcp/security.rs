//! Security-report delivery.

use std::{io::Write as _, str::FromStr as _};

use age::{Encryptor, armor, x25519};

use crate::{
    agent::Publisher,
    manifest::SecurityPolicy,
    mcp::{
        protocol::{
            CreateIssueRequest, SecurityIssueRequest, SecurityIssueResult, mutation_identity,
        },
        tools::ToolService,
    },
};

#[derive(Debug, Clone)]
pub(super) struct SecurityReporting {
    publisher: Publisher,
}

impl SecurityReporting {
    pub(super) fn new(publisher: &Publisher) -> Self {
        Self {
            publisher: publisher.clone(),
        }
    }

    pub(super) async fn report(
        &self,
        service: &ToolService,
        request: SecurityIssueRequest,
    ) -> Result<SecurityIssueResult, String> {
        let policy = self
            .publisher
            .security_policy()
            .await
            .map_err(|error| error.to_string())?;
        let request_id = mutation_identity("report_security_issue", &request)
            .map_err(|error| error.to_string())?;
        let description = Self::encrypt(&policy, &request)?;
        let publisher = self.publisher.for_repository(policy.repository());
        let issue = service
            .for_repository(policy.repository(), publisher)
            .create_security_issue_write(
                CreateIssueRequest {
                    title: "Encrypted security report".to_owned(),
                    description,
                    labels: vec!["security".to_owned()],
                    assignees: Vec::new(),
                },
                request_id,
            )
            .await
            .map_err(|error| error.to_string())?;
        let issue = issue
            .0
            .data
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "security report returned no issue ID".to_owned())?
            .to_owned();

        Ok(SecurityIssueResult {
            repository: policy.repository().to_string(),
            issue,
        })
    }

    fn encrypt(policy: &SecurityPolicy, request: &SecurityIssueRequest) -> Result<String, String> {
        let recipients = policy
            .recipients()
            .iter()
            .map(|recipient| x25519::Recipient::from_str(recipient))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        let encryptor = Encryptor::with_recipients(
            recipients
                .iter()
                .map(|recipient| recipient as &dyn age::Recipient),
        )
        .map_err(|error| error.to_string())?;
        let plaintext = serde_json::to_vec(request).map_err(|error| error.to_string())?;
        let mut encrypted = Vec::new();
        let armor = armor::ArmoredWriter::wrap_output(&mut encrypted, armor::Format::AsciiArmor)
            .map_err(|error| error.to_string())?;
        let mut writer = encryptor
            .wrap_output(armor)
            .map_err(|error| error.to_string())?;
        writer
            .write_all(&plaintext)
            .map_err(|error| error.to_string())?;
        writer
            .finish()
            .and_then(armor::ArmoredWriter::finish)
            .map_err(|error| error.to_string())?;

        String::from_utf8(encrypted).map_err(|error| error.to_string())
    }
}
