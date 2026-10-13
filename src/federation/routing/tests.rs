use super::validate;

pub(crate) fn validate_probe(
    request: &crate::federation::control::SynthesizeRequest,
    descriptor: &crate::federation::catalog::ModelDescriptor,
    events: &[crate::federation::control::SynthesizeEvent],
) -> Result<(), rama::http::grpc::Status> {
    let mut validation = validate::Synthesis::new(request, descriptor)?;
    for event in events {
        validation.event(event)?;
    }
    validation.finish(Ok(())).map(drop)
}
