use std::process::Command;

pub fn scan_image(image_path: &str) -> Result<String, String> {
    // Execute tesseract CLI
    // tesseract <image_path> stdout
    let output = Command::new("tesseract")
        .arg(image_path)
        .arg("stdout") // Output to stdout
        .output()
        .map_err(|e| format!("Failed to execute tesseract: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Tesseract failed: {}", stderr));
    }

    let text =
        String::from_utf8(output.stdout).map_err(|e| format!("Invalid UTF-8 output: {}", e))?;

    Ok(text.trim().to_string())
}
