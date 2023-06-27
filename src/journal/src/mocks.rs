use crate::raw::{PlainRawEntry, RawEntryHeader};
use crate::EntryEnricher;
use restate_common::types::{
    EnrichedEntryHeader, EnrichedRawEntry, RawEntry, ResolutionResult, ServiceInvocationSpanContext,
};
use restate_common::utils::GenericError;

#[derive(Debug, Default, Clone)]
pub struct MockEntryEnricher;

impl EntryEnricher for MockEntryEnricher {
    fn enrich_entry(
        &self,
        raw_entry: PlainRawEntry,
        invocation_span_context: &ServiceInvocationSpanContext,
    ) -> Result<EnrichedRawEntry, GenericError> {
        let enriched_header = match raw_entry.header {
            RawEntryHeader::PollInputStream { is_completed } => {
                EnrichedEntryHeader::PollInputStream { is_completed }
            }
            RawEntryHeader::OutputStream => EnrichedEntryHeader::OutputStream,
            RawEntryHeader::GetState { is_completed } => {
                EnrichedEntryHeader::GetState { is_completed }
            }
            RawEntryHeader::SetState => EnrichedEntryHeader::SetState,
            RawEntryHeader::ClearState => EnrichedEntryHeader::ClearState,
            RawEntryHeader::Sleep { is_completed } => EnrichedEntryHeader::Sleep { is_completed },
            RawEntryHeader::Invoke { is_completed } => {
                if !is_completed {
                    EnrichedEntryHeader::Invoke {
                        is_completed,
                        resolution_result: Some(ResolutionResult {
                            invocation_id: Default::default(),
                            service_key: Default::default(),
                            span_context: invocation_span_context.clone(),
                        }),
                    }
                } else {
                    // No need to service resolution if the entry was completed by the service endpoint
                    EnrichedEntryHeader::Invoke {
                        is_completed,
                        resolution_result: None,
                    }
                }
            }
            RawEntryHeader::BackgroundInvoke => EnrichedEntryHeader::BackgroundInvoke {
                resolution_result: ResolutionResult {
                    invocation_id: Default::default(),
                    service_key: Default::default(),
                    span_context: invocation_span_context.clone(),
                },
            },
            RawEntryHeader::Awakeable { is_completed } => {
                EnrichedEntryHeader::Awakeable { is_completed }
            }
            RawEntryHeader::CompleteAwakeable => EnrichedEntryHeader::CompleteAwakeable,
            RawEntryHeader::Custom { code, requires_ack } => {
                EnrichedEntryHeader::Custom { code, requires_ack }
            }
        };

        Ok(RawEntry::new(enriched_header, raw_entry.entry))
    }
}