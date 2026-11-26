//! Build script:
//! * compiles the Slint UI,
//! * decodes `Finished.mp3` to an in-memory WAV for `PlaySound`,
//! * renders the application icon from SVG and the per-format icons from PNG,
//! * embeds icons, version info and the application manifest (`rc.exe` on Windows hosts,
//!   `llvm-rc` or `$RC_PATH` when cross-compiling).

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const ICON_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];
const FIRST_FORMAT_ICON_ID: u16 = 101;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest_dir.join("../..").canonicalize().unwrap();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target_windows = env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=ui");
    for asset in ["assets/archive-formats.json", "Finished.mp3", "appicon", "icons"] {
        println!("cargo:rerun-if-changed={}", root.join(asset).display());
    }

    if target_windows {
        let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
        slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI compiles");
    }

    write_wave(&root.join("Finished.mp3"), &out.join("finished.wav"));

    let keys = icon_keys(&root.join("assets/archive-formats.json"));
    let ids: Vec<(String, u16)> = keys.iter().enumerate().map(|(i, k)| (k.clone(), FIRST_FORMAT_ICON_ID + i as u16)).collect();
    let table: String = ids.iter().map(|(k, id)| format!("    ({k:?}, {id}),\n")).collect();
    fs::write(out.join("icon_ids.rs"), format!("pub const FORMAT_ICON_IDS: &[(&str, u16)] = &[\n{table}];\n")).unwrap();

    // Resources are part of the product (icons referenced by the associations, DPI and
    // long-path manifest), so a Windows build without them is an error, not a fallback.
    if target_windows {
        let icons = out.join("icons");
        fs::create_dir_all(&icons).unwrap();
        let app_icon = icons.join("app.ico");
        write_app_icon(&root.join("appicon"), &app_icon);

        let mut res = winresource::WindowsResource::new();
        res.set_icon(app_icon.to_str().unwrap());
        for (key, id) in &ids {
            let ico = icons.join(format!("{key}.ico"));
            let png = root.join("icons").join(format!("{key}.png"));
            let source = if png.exists() { png } else { root.join("icons/origin.png") };
            write_png_icon(&source, &ico);
            res.set_icon_with_id(ico.to_str().unwrap(), &id.to_string());
        }
        let version = env::var("CARGO_PKG_VERSION").unwrap();
        res.set("FileDescription", "Unbox - 双击即解压")
            .set("ProductName", "Unbox")
            .set("CompanyName", "Unbox")
            .set("OriginalFilename", "unbox.exe")
            .set("LegalCopyright", "MIT License")
            .set("FileVersion", &version)
            .set("ProductVersion", &version)
            .set_manifest(MANIFEST);
        res.compile().expect("resource compilation");
    }
}

fn icon_keys(catalog: &Path) -> Vec<String> {
    let text = fs::read_to_string(catalog).unwrap();
    let mut keys = BTreeSet::new();
    keys.insert("origin".to_string());
    // A tiny scan is enough here; the catalog is validated again at runtime.
    for part in text.split("\"iconKey\":").skip(1) {
        let value = part.trim_start().trim_start_matches('"');
        let key: String = value.chars().take_while(|c| *c != '"').collect();
        assert!(!key.is_empty() && key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()), "bad icon key {key}");
        keys.insert(key);
    }
    keys.into_iter().collect()
}

fn write_wave(mp3: &Path, wav: &Path) {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let file = Box::new(fs::File::open(mp3).expect("Finished.mp3"));
    let stream = MediaSourceStream::new(file, Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp3");
    let mut format =
        symphonia::default::get_probe().probe(&hint, stream, FormatOptions::default(), MetadataOptions::default()).expect("probe mp3");
    let track = format.default_track(TrackType::Audio).expect("audio track");
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(track.codec_params.as_ref().unwrap().audio().unwrap(), &AudioDecoderOptions::default())
        .expect("mp3 decoder");

    let mut pcm: Vec<i16> = Vec::new();
    let (mut rate, mut channels) = (44100u32, 2u16);
    let mut scratch: Vec<i16> = Vec::new();
    while let Ok(Some(packet)) = format.next_packet() {
        if packet.track_id != track_id {
            continue;
        }
        if let Ok(buffer) = decoder.decode(&packet) {
            rate = buffer.spec().rate();
            channels = buffer.spec().channels().count() as u16;
            scratch.resize(buffer.samples_interleaved(), 0);
            buffer.copy_to_slice_interleaved(&mut scratch);
            pcm.extend_from_slice(&scratch);
        }
    }

    let data_len = (pcm.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + data_len as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    bytes.extend_from_slice(&(channels * 2).to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in pcm {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    fs::write(wav, bytes).unwrap();
}

fn write_ico(frames: Vec<(u32, Vec<u8>)>, path: &Path) {
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for (size, rgba) in frames {
        let image = ico::IconImage::from_rgba_data(size, size, rgba);
        dir.add_entry(ico::IconDirEntry::encode(&image).unwrap());
    }
    dir.write(fs::File::create(path).unwrap()).unwrap();
}

fn render_svg(svg: &Path, size: u32) -> Vec<u8> {
    let data = fs::read(svg).unwrap();
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).expect("valid svg");
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).unwrap();
    let scale = size as f32 / tree.size().width().max(tree.size().height());
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect()
}

fn write_app_icon(dir: &Path, ico: &Path) {
    let frames = ICON_SIZES
        .iter()
        .map(|&size| {
            let svg = if size <= 24 { dir.join("package-box-24px.svg") } else { dir.join("package-box.svg") };
            (size, render_svg(&svg, size))
        })
        .collect();
    write_ico(frames, ico);
}

fn write_png_icon(png: &Path, ico: &Path) {
    let source = image::open(png).expect("icon png").to_rgba8();
    let frames = ICON_SIZES
        .iter()
        .map(|&size| {
            let scaled = image::imageops::resize(&source, size, size, image::imageops::FilterType::Lanczos3);
            (size, scaled.into_raw())
        })
        .collect();
    write_ico(frames, ico);
}

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="Unbox" version="2.0.0.0"/>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0"
        processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
    </windowsSettings>
  </application>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>
"#;
