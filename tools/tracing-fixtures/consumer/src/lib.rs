use facade::errors::RiftError;

pub fn expression() -> u8 {
    facade::tracing::traced!("fixture.expression", 1 + 2)
}

pub fn block(calls: &mut u8) -> u8 {
    facade::tracing::traced!("fixture.block", {
        *calls += 1;
        7
    })
}

pub fn question(fail: bool) -> Result<u8, RiftError> {
    facade::tracing::traced!("fixture.question", {
        let value = if fail { Err(refusal()) } else { Ok(9) }?;
        Ok(value)
    })
}

pub fn early_return(fail: bool) -> Result<u8, RiftError> {
    let value = facade::tracing::traced!("fixture.return", {
        if fail {
            return Err(refusal());
        }
        11
    });
    Ok(value)
}

pub fn parent(parent: &facade::tracing::Span) -> u8 {
    facade::tracing::traced!(
        parent: parent,
        component = "fixture",
        operation = "fixture.parent",
        units = 3,
        { 13 }
    )
}

pub fn loop_exits() -> Vec<u8> {
    let mut visited = Vec::new();
    for value in 0..8 {
        facade::tracing::traced!("fixture.loop", {
            if value % 2 == 0 {
                continue;
            }
            if value > 5 {
                break;
            }
            visited.push(value);
        });
    }
    visited
}

pub async fn asynchronous(fail: bool) -> Result<u8, RiftError> {
    facade::tracing::traced!("fixture.async", async move {
        let value = if fail { Err(refusal()) } else { Ok(17) }?;
        Ok(value)
    })
    .await
}

fn refusal() -> RiftError {
    facade::errors::errors::tracing::log_store_failed()
        .operation("read")
        .path(std::path::Path::new("fixture"))
        .detail("refused")
        .error()
}
