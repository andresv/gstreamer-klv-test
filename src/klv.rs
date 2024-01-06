use anyhow::Result;
use gst::{element_error, prelude::*, Caps};
use gstreamer as gst;
use gstreamer_app as gst_app;

pub fn klv_sink() -> Result<gst::Element> {
    let appsink = gst_app::AppSink::builder()
        .caps(&Caps::builder("meta/x-klv").field("parsed", true).build())
        .build();

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            // Add a handler to the "new-sample" signal.
            .new_sample(|appsink| {
                // Pull the sample in question out of the appsink's buffer.
                let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or_else(|| {
                    element_error!(
                        appsink,
                        gst::ResourceError::Failed,
                        ("Failed to get buffer from appsink")
                    );
                    gst::FlowError::Error
                })?;

                if buffer.size() > 0 {
                    let mr = buffer.map_readable().unwrap();
                    log::info!("receive klv {:?}", mr.as_slice());
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    Ok(appsink.upcast::<gst::Element>())
}

pub fn klv_test_src() -> Result<gst::Element> {
    let appsrc = gst_app::AppSrc::builder()
        .caps(&Caps::builder("meta/x-klv").field("parsed", true).build())
        .format(gst::Format::Time)
        .build();

    // This is not needed, actually data is generated in frame callback.

    // let mut i = 0;
    // appsrc.set_callbacks(
    //     gst_app::AppSrcCallbacks::builder()
    //         .need_data(move |appsrc, _| {
    //             let data = [0x2A, 0x02, 0x00, 0x03];

    //             let mut buffer = gst::Buffer::with_size(data.len()).unwrap();
    //             {
    //                 let bufref = buffer.make_mut();
    //                 bufref.set_pts(i * 500 * gst::ClockTime::MSECOND);
    //                 let mut mw = bufref.map_writable().unwrap();
    //                 mw.as_mut_slice().copy_from_slice(&data)
    //             }

    //             info!("sending buffer: {}", i);
    //             i += 1;

    //             // appsrc already handles the error here for us.
    //             let _ = appsrc.push_buffer(buffer);
    //         })
    //         .build(),
    // );

    Ok(appsrc.upcast::<gst::Element>())
}
