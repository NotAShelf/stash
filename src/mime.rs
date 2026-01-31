use imagesize::ImageType;

/// Detect MIME type of clipboard data. We try binary detection first using
/// [`imagesize`] followed by a check for text/uri-list for file manager copies
/// and finally fall back to text/plain for UTF-8 or [`None`] for binary.
pub fn detect_mime(data: &[u8]) -> Option<String> {
  if data.is_empty() {
    return None;
  }

  // Try image detection first
  if let Ok(img_type) = imagesize::image_type(data) {
    return Some(image_type_to_mime(img_type));
  }

  // Check if it's UTF-8 text
  if let Ok(text) = std::str::from_utf8(data) {
    let trimmed = text.trim();

    // Check for text/uri-list format (file paths from file managers)
    if is_uri_list(trimmed) {
      return Some("text/uri-list".to_string());
    }

    // Default to plain text
    return Some("text/plain".to_string());
  }

  // Unknown binary data
  None
}

/// Convert [`imagesize`] [`ImageType`] to MIME type string
fn image_type_to_mime(img_type: ImageType) -> String {
  let mime = match img_type {
    ImageType::Png => "image/png",
    ImageType::Jpeg => "image/jpeg",
    ImageType::Gif => "image/gif",
    ImageType::Bmp => "image/bmp",
    ImageType::Tiff => "image/tiff",
    ImageType::Webp => "image/webp",
    ImageType::Aseprite => "image/x-aseprite",
    ImageType::Dds => "image/vnd.ms-dds",
    ImageType::Exr => "image/aces",
    ImageType::Farbfeld => "image/farbfeld",
    ImageType::Hdr => "image/vnd.radiance",
    ImageType::Ico => "image/x-icon",
    ImageType::Ilbm => "image/ilbm",
    ImageType::Jxl => "image/jxl",
    ImageType::Ktx2 => "image/ktx2",
    ImageType::Pnm => "image/x-portable-anymap",
    ImageType::Psd => "image/vnd.adobe.photoshop",
    ImageType::Qoi => "image/qoi",
    ImageType::Tga => "image/x-tga",
    ImageType::Vtf => "image/x-vtf",
    ImageType::Heif(imagesize::Compression::Hevc) => "image/heic",
    ImageType::Heif(_) => "image/heif",
    _ => "application/octet-stream",
  };
  mime.to_string()
}

/// Check if text is a URI list per RFC 2483.
///
/// Used when copying files from file managers - they provide file paths
/// as text/uri-list format (`file://` URIs, one per line, `#` for comments).
fn is_uri_list(text: &str) -> bool {
  if text.is_empty() {
    return false;
  }

  // Must start with a URI scheme to even consider it
  if !text.starts_with("file://")
    && !text.starts_with("http://")
    && !text.starts_with("https://")
    && !text.starts_with("ftp://")
    && !text.starts_with('#')
  {
    return false;
  }

  let lines: Vec<&str> = text.lines().map(str::trim).collect();

  // Check first non-comment line is a URI
  let first_content =
    lines.iter().find(|l| !l.is_empty() && !l.starts_with('#'));

  if let Some(line) = first_content {
    line.starts_with("file://")
      || line.starts_with("http://")
      || line.starts_with("https://")
      || line.starts_with("ftp://")
  } else {
    false
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_empty_data() {
    assert_eq!(detect_mime(b""), None);
  }

  #[test]
  fn test_plain_text() {
    let data = b"Hello, world!";
    assert_eq!(detect_mime(data), Some("text/plain".to_string()));
  }

  #[test]
  fn test_uri_list_single_file() {
    let data = b"file:///home/user/document.pdf";
    assert_eq!(detect_mime(data), Some("text/uri-list".to_string()));
  }

  #[test]
  fn test_uri_list_multiple_files() {
    let data = b"file:///home/user/file1.txt\nfile:///home/user/file2.txt";
    assert_eq!(detect_mime(data), Some("text/uri-list".to_string()));
  }

  #[test]
  fn test_uri_list_with_comments() {
    let data = b"# Comment\nfile:///home/user/file.txt";
    assert_eq!(detect_mime(data), Some("text/uri-list".to_string()));
  }

  #[test]
  fn test_uri_list_http() {
    let data = b"https://example.com/image.png";
    assert_eq!(detect_mime(data), Some("text/uri-list".to_string()));
  }

  #[test]
  fn test_not_uri_list() {
    let data = b"This is just text with file:// in the middle";
    assert_eq!(detect_mime(data), Some("text/plain".to_string()));
  }

  #[test]
  fn test_unknown_binary() {
    // Binary data that's not UTF-8 and not a known format
    let data = b"\x80\x81\x82\x83\x84\x85\x86\x87";
    assert_eq!(detect_mime(data), None);
  }

  #[test]
  fn test_uri_list_trailing_newline() {
    let data = b"file:///foo\n";
    assert_eq!(detect_mime(data), Some("text/uri-list".to_string()));
  }

  #[test]
  fn test_uri_list_ftp() {
    let data = b"ftp://host/path";
    assert_eq!(detect_mime(data), Some("text/uri-list".to_string()));
  }

  #[test]
  fn test_uri_list_mixed_schemes() {
    let data = b"file:///home/user/doc.pdf\nhttps://example.com/file.zip";
    assert_eq!(detect_mime(data), Some("text/uri-list".to_string()));
  }

  #[test]
  fn test_plain_url_in_text() {
    let data = b"visit http://example.com for info";
    assert_eq!(detect_mime(data), Some("text/plain".to_string()));
  }

  #[test]
  fn test_png_magic_bytes() {
    // Real PNG header: 8-byte signature + minimal IHDR chunk
    let data: &[u8] = &[
      0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
      0x00, 0x00, 0x00, 0x0D, // IHDR chunk length
      0x49, 0x48, 0x44, 0x52, // "IHDR"
      0x00, 0x00, 0x00, 0x01, // width: 1
      0x00, 0x00, 0x00, 0x01, // height: 1
      0x08, 0x02, // bit depth: 8, color type: 2 (RGB)
      0x00, 0x00, 0x00, // compression, filter, interlace
      0x90, 0x77, 0x53, 0xDE, // CRC
    ];
    assert_eq!(detect_mime(data), Some("image/png".to_string()));
  }

  #[test]
  fn test_jpeg_magic_bytes() {
    // JPEG SOI marker + APP0 (JFIF) marker
    let data: &[u8] = &[
      0xFF, 0xD8, 0xFF, 0xE0, // SOI + APP0
      0x00, 0x10, // Length
      0x4A, 0x46, 0x49, 0x46, 0x00, // "JFIF\0"
      0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00,
    ];
    assert_eq!(detect_mime(data), Some("image/jpeg".to_string()));
  }

  #[test]
  fn test_gif_magic_bytes() {
    // GIF89a header
    let data: &[u8] = &[
      0x47, 0x49, 0x46, 0x38, 0x39, 0x61, // "GIF89a"
      0x01, 0x00, 0x01, 0x00, // 1x1
      0x80, 0x00, 0x00, // GCT flag, bg, aspect
    ];
    assert_eq!(detect_mime(data), Some("image/gif".to_string()));
  }

  #[test]
  fn test_webp_magic_bytes() {
    // RIFF....WEBP header
    let data: &[u8] = &[
      0x52, 0x49, 0x46, 0x46, // "RIFF"
      0x24, 0x00, 0x00, 0x00, // file size
      0x57, 0x45, 0x42, 0x50, // "WEBP"
      0x56, 0x50, 0x38, 0x20, // "VP8 "
      0x18, 0x00, 0x00, 0x00, // chunk size
      0x30, 0x01, 0x00, 0x9D, 0x01, 0x2A, // VP8 bitstream
      0x01, 0x00, 0x01, 0x00, // width/height
    ];
    assert_eq!(detect_mime(data), Some("image/webp".to_string()));
  }

  #[test]
  fn test_whitespace_only() {
    let data = b"   \n\t  ";
    // Valid UTF-8 text, even if only whitespace. [`detect_mime`] doesn't reject
    // it (store_entry rejects it separately). As text it's text/plain.
    assert_eq!(detect_mime(data), Some("text/plain".to_string()));
  }

  #[test]
  fn test_image_type_to_mime_coverage() {
    assert_eq!(image_type_to_mime(ImageType::Png), "image/png");
    assert_eq!(image_type_to_mime(ImageType::Jpeg), "image/jpeg");
    assert_eq!(image_type_to_mime(ImageType::Gif), "image/gif");
    assert_eq!(image_type_to_mime(ImageType::Bmp), "image/bmp");
    assert_eq!(image_type_to_mime(ImageType::Tiff), "image/tiff");
    assert_eq!(image_type_to_mime(ImageType::Webp), "image/webp");
    assert_eq!(image_type_to_mime(ImageType::Aseprite), "image/x-aseprite");
    assert_eq!(image_type_to_mime(ImageType::Dds), "image/vnd.ms-dds");
    assert_eq!(image_type_to_mime(ImageType::Exr), "image/aces");
    assert_eq!(image_type_to_mime(ImageType::Farbfeld), "image/farbfeld");
    assert_eq!(image_type_to_mime(ImageType::Hdr), "image/vnd.radiance");
    assert_eq!(image_type_to_mime(ImageType::Ico), "image/x-icon");
    assert_eq!(image_type_to_mime(ImageType::Ilbm), "image/ilbm");
    assert_eq!(image_type_to_mime(ImageType::Jxl), "image/jxl");
    assert_eq!(image_type_to_mime(ImageType::Ktx2), "image/ktx2");
    assert_eq!(
      image_type_to_mime(ImageType::Pnm),
      "image/x-portable-anymap"
    );
    assert_eq!(
      image_type_to_mime(ImageType::Psd),
      "image/vnd.adobe.photoshop"
    );
    assert_eq!(image_type_to_mime(ImageType::Qoi), "image/qoi");
    assert_eq!(image_type_to_mime(ImageType::Tga), "image/x-tga");
    assert_eq!(image_type_to_mime(ImageType::Vtf), "image/x-vtf");
    assert_eq!(
      image_type_to_mime(ImageType::Heif(imagesize::Compression::Hevc)),
      "image/heic"
    );
    assert_eq!(
      image_type_to_mime(ImageType::Heif(imagesize::Compression::Av1)),
      "image/heif"
    );
  }
}
