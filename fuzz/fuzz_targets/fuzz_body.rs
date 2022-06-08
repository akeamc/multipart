#![no_main]
use bytes::Bytes;
use futures::{future, stream, StreamExt};
use libfuzzer_sys::fuzz_target;
use multipart::{Boundary, Multipart};
use tokio::runtime;

fuzz_target!(|body: &[u8]| {
    let body = body.to_vec();
    let stream = stream::once(future::ready(Result::<Bytes, ()>::Ok(Bytes::from(body))));

    let boundary = Boundary::new("xoxo");

    let rt = runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");

    let mut multipart = Multipart::from_body(stream, boundary);

    rt.block_on(async {
        while let Some(res) = multipart.next().await {
            if let Ok(mut field) = res {
                while let Some(res) = field.next().await {
                    if let Ok(_chunk) = res {
                        // success
                    }
                }
            }
        }
    });
});
