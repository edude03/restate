// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::HashMap;

use cling::prelude::*;
use comfy_table::Table;

use crate::cli_env::CliEnv;
use crate::console::c_println;
use crate::meta_client::{MetaClient, MetaClientInterface};
use crate::ui::console::{Styled, StyledTable};
use crate::ui::stylesheet::Style;
use restate_meta::rest_api::endpoints::{ServiceEndpoint, ServiceEndpointResponse};

use anyhow::Result;

#[derive(Run, Parser, Collect, Clone)]
#[cling(run = "run_describe")]
pub struct Describe {
    /// Service name
    name: String,
}

pub async fn run_describe(State(env): State<CliEnv>, describe_opts: &Describe) -> Result<()> {
    let client = MetaClient::new(&env)?;
    let svc = client
        .get_service(&describe_opts.name)
        .await?
        .into_body()
        .await?;

    let mut table = Table::new_styled(&env.ui_config);
    table.add_row(vec!["Name:", svc.name.as_ref()]);
    table.add_row(vec![
        "Flavor (Instance Type):",
        &format!("{:?}", svc.instance_type),
    ]);
    table.add_row(vec!["Revision:", &svc.revision.to_string()]);
    table.add_row(vec!["Public:", &svc.public.to_string()]);
    table.add_row(vec!["Endpoint Id:", &svc.endpoint_id.to_string()]);

    let endpoint = client
        .get_endpoint(&svc.endpoint_id)
        .await?
        .into_body()
        .await?;
    add_endpoint(&endpoint, &mut table);

    c_println!("{}", Styled(Style::Info, "Service Information"));
    c_println!("{}", table);

    // Methods
    c_println!();
    c_println!("{}", Styled(Style::Info, "Methods"));
    let mut table = Table::new_styled(&env.ui_config);
    table.set_header(vec!["NAME", "INPUT TYPE", "OUTPUT TYPE", "KEY FIELD INDEX"]);
    for method in svc.methods {
        table.add_row(vec![
            &method.name,
            &method.input_type,
            &method.output_type,
            &method
                .key_field_number
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
        ]);
    }
    c_println!("{}", table);

    Ok(())
}

fn add_endpoint(endpoint: &ServiceEndpointResponse, table: &mut Table) {
    let additional_headers = match &endpoint.service_endpoint {
        ServiceEndpoint::Http {
            uri,
            protocol_type,
            additional_headers,
        } => {
            table.add_row(vec!["Endpoint Type:", "HTTP"]);
            table.add_row(vec!["Endpoint URL:", &uri.to_string()]);
            let protocol_type = match protocol_type {
                restate_schema_api::endpoint::ProtocolType::RequestResponse => "RequestResponse",
                restate_schema_api::endpoint::ProtocolType::BidiStream => "BidiStream",
            }
            .to_string();
            table.add_row(vec!["Endpoint Protocol:", &protocol_type]);
            additional_headers.clone()
        }
        ServiceEndpoint::Lambda {
            arn,
            assume_role_arn,
            additional_headers,
        } => {
            table.add_row(vec!["Endpoint Type:", "AWS Lambda"]);
            table.add_row(vec!["Endpoint ARN:", &arn.to_string()]);
            table.add_row_if(
                |_, _| assume_role_arn.is_some(),
                vec![
                    "Endpoint Assume Role ARN:",
                    assume_role_arn.as_ref().unwrap(),
                ],
            );
            additional_headers.clone()
        }
    };

    let additional_headers: HashMap<http::HeaderName, http::HeaderValue> =
        additional_headers.into();

    for (header, value) in additional_headers.iter() {
        table.add_row(vec![
            "Endpoint Additional Header:",
            &format!("{}: {}", header, value.to_str().unwrap_or("<BINARY>")),
        ]);
    }
}
