use super::{Failure, Info, Job, Stage, rejected};
use std::process::Command;
pub(super) fn dimensions(info: &Info, edge: u32) -> (u32, u32) {
    let edge = edge.min(info.width.max(info.height));
    let long = u64::from(info.width.max(info.height));
    let even =
        |n| (u32::try_from(u64::from(n) * u64::from(edge) / long).unwrap_or(2) / 2 * 2).max(2);
    (even(info.width), even(info.height))
}
pub(super) fn av1_dimensions(info: &Info) -> (u32, u32) {
    let long = info.width.max(info.height);
    let short = info.width.min(info.height).max(1);
    let edge = u32::try_from((u64::from(long) * 2160 / u64::from(short)).min(3840)).unwrap_or(3840);
    dimensions(info, edge)
}
#[allow(clippy::too_many_lines)] // Keep the complete parser command policy together for review.
pub(super) fn build(job: &Job, stage: Stage, info: Option<&Info>) -> Result<Command, Failure> {
    let probe = matches!(stage, Stage::Probe | Stage::Av1Probe);
    let mut cmd = Command::new(if probe {
        &job.settings.ffprobe
    } else {
        &job.settings.ffmpeg
    });
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("OMP_NUM_THREADS", job.settings.threads.to_string());
    cmd.args([
        "-v",
        "error",
        "-max_alloc",
        "268435456",
        "-threads",
        &job.settings.threads.to_string(),
    ]);
    if !probe {
        cmd.args(["-nostdin", "-n", "-filter_threads", "1"]);
    }
    cmd.args([
        "-protocol_whitelist",
        "file",
        "-format_whitelist",
        "mov",
        "-codec_whitelist",
        if matches!(stage, Stage::Av1Probe) {
            "av1,libdav1d,aac"
        } else {
            "h264,hevc,aac,pcm_s16le,pcm_s24le,alac"
        },
        "-probesize",
        "5000000",
        "-analyzeduration",
        "5000000",
        "-f",
        "mov",
    ]);
    if probe {
        if matches!(stage, Stage::Av1Probe) {
            cmd.arg(job.out_dir.join("av1.mp4")).args([
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=profile,level,pix_fmt",
                "-of",
                "json",
            ]);
        } else {
            cmd.arg(&job.input).args(["-show_entries", "format=duration:stream=codec_type,codec_name,width,height,sample_aspect_ratio,color_transfer:stream_side_data=rotation", "-of", "json"]);
        }
        return Ok(cmd);
    }
    let info = info.ok_or_else(rejected)?;
    if matches!(stage, Stage::Poster) {
        cmd.args([
            "-ss",
            &format!(
                "{}.{:04}",
                info.duration_ms / 10_000,
                info.duration_ms % 10_000
            ),
        ]);
    }
    #[cfg(feature = "test-hooks")]
    if std::env::args().any(|s| s == "--video-realtime") {
        cmd.arg("-re");
    }
    cmd.arg("-i").arg(&job.input).args([
        "-map_metadata",
        "-1",
        "-map_chapters",
        "-1",
        "-map",
        "0:v:0",
    ]);
    let tone = if info.hdr {
        "zscale=t=linear:npl=100,format=gbrpf32le,tonemap=tonemap=hable:desat=0,zscale=p=bt709:t=bt709:m=bt709:r=tv,"
    } else {
        ""
    };
    let name = match stage {
        Stage::H264 | Stage::Av1 => {
            let h264 = matches!(stage, Stage::H264);
            let (w, h) = if h264 {
                dimensions(info, 1080)
            } else {
                av1_dimensions(info)
            };
            cmd.args([
                "-map",
                "0:a:0?",
                "-vf",
                &format!("{tone}scale={w}:{h}:flags=lanczos,setsar=1,format=yuv420p"),
                "-c:v",
                if h264 { "libx264" } else { "libsvtav1" },
                "-crf",
                if h264 { "21" } else { "30" },
                "-threads",
                &job.settings.threads.to_string(),
            ]);
            if h264 {
                cmd.args(["-profile:v", "high", "-preset", "medium"]);
            } else {
                cmd.args([
                    "-preset",
                    "8",
                    "-svtav1-params",
                    &format!("lp={}:tier=0", job.settings.threads),
                ]);
            }
            cmd.args([
                "-c:a",
                "aac",
                "-b:a",
                "160k",
                "-color_primaries",
                "bt709",
                "-color_trc",
                "bt709",
                "-colorspace",
                "bt709",
                "-movflags",
                "+faststart",
                "-f",
                "mp4",
            ]);
            if h264 { "h264.mp4" } else { "av1.mp4" }
        }
        Stage::Poster => {
            cmd.args([
                "-an",
                "-vf",
                &format!(
                    "{tone}scale={}:{}:flags=lanczos,setsar=1,format=yuvj420p",
                    info.width, info.height
                ),
                "-frames:v",
                "1",
                "-c:v",
                "mjpeg",
                "-f",
                "image2",
                "-update",
                "1",
            ]);
            "poster.jpeg"
        }
        Stage::Scrub => {
            let (w, h) = dimensions(info, 160);
            cmd.args(["-an", "-vf", &format!("fps=100000/{}:start_time=0,{tone}scale={w}:{h}:flags=lanczos,setsar=1,pad=160:160:(ow-iw)/2:(oh-ih)/2,tile=10x10:nb_frames=100:padding=0:margin=0", info.duration_ms), "-frames:v", "1", "-c:v", "mjpeg", "-f", "image2", "-update", "1"]);
            "scrub.jpeg"
        }
        Stage::Probe | Stage::Av1Probe => return Err(rejected()),
    };
    cmd.arg(job.out_dir.join(name));
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn four_k_av1_preserves_landscape_and_portrait_and_caps_eight_k() {
        for (width, height, expected) in [
            (3840, 2160, (3840, 2160)),
            (2160, 3840, (2160, 3840)),
            (7680, 4320, (3840, 2160)),
            (1920, 1080, (1920, 1080)),
        ] {
            let info = Info {
                width,
                height,
                duration_ms: 1000,
                hdr: false,
            };
            assert_eq!(av1_dimensions(&info), expected);
            assert!(dimensions(&info, 1080).0.max(dimensions(&info, 1080).1) <= 1080);
        }
    }
}
