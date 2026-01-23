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
}
