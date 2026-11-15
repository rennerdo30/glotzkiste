//! Glotzkiste: several live streams and chats at once (docs/DESIGN.md).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod libmpv;
mod logger;
mod stream;

const HELP: &str = "Glotzkiste: a multi-stream live viewer.

Usage: glotzkiste [options] [CHANNEL...]

  --mute            Start every stream muted
  --smoke SECS      Run offscreen for SECS seconds, print measurements, exit
  --snapshot FILE   With --smoke: save a PNG of the window at the end
  --size WxH        Window size in pixels (default 1600x900)
  --help            This text
";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Options {
    pub channels: Vec<String>,
    pub mute: bool,
    pub smoke: Option<f64>,
    pub snapshot: Option<std::path::PathBuf>,
    pub size: Option<(u32, u32)>,
}

fn parse(args: &[String]) -> Result<Option<Options>, String> {
    let mut o = Options::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--help" | "-h" => return Ok(None),
            "--mute" => o.mute = true,
            "--smoke" => {
                o.smoke = Some(
                    it.next().and_then(|s| s.parse::<f64>().ok()).filter(|s| *s > 0.0 && *s <= 600.0).ok_or("--smoke needs seconds (up to 600)")?,
                )
            }
            "--snapshot" => o.snapshot = Some(it.next().ok_or("--snapshot needs a file")?.into()),
            "--size" => {
                let s = it.next().ok_or("--size needs WxH")?;
                let (w, h) = s.split_once('x').ok_or("--size needs WxH")?;
                o.size = Some((w.parse().map_err(|_| "bad width")?, h.parse().map_err(|_| "bad height")?));
            }
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            s => {
                let login = s.trim().trim_start_matches("https://").trim_start_matches("www.").trim_start_matches("twitch.tv/").to_ascii_lowercase();
                if !gk_twitch::api::valid_login(&login) {
                    return Err(format!("{s} is not a channel name"));
                }
                o.channels.push(login);
            }
        }
    }
    Ok(Some(o))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse(&args) {
        Ok(Some(o)) => o,
        Ok(None) => {
            print!("{HELP}");
            return;
        }
        Err(e) => {
            eprintln!("glotzkiste: {e}\n\n{HELP}");
            std::process::exit(2);
        }
    };
    logger::init(options.smoke.is_some());
    std::process::exit(app::run(options));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_channels_and_flags() {
        let o = parse(&a(&["--mute", "Some_One", "https://www.twitch.tv/other", "--smoke", "5"])).unwrap().unwrap();
        assert_eq!(o.channels, ["some_one", "other"]);
        assert!(o.mute);
        assert_eq!(o.smoke, Some(5.0));
        assert!(parse(&a(&["bad name!"])).is_err());
        assert!(parse(&a(&["--smoke", "0"])).is_err());
        assert_eq!(parse(&a(&["--size", "800x600"])).unwrap().unwrap().size, Some((800, 600)));
    }
}
