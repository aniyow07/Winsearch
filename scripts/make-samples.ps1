# Creates sample files for trying the preview pane: a 3-page PDF, a Word
# document, an Excel workbook, a large PNG, and a text file.
#   powershell -ExecutionPolicy Bypass -File scripts\make-samples.ps1 [-Out samples]
param([string]$Out = (Join-Path $PSScriptRoot "..\samples"))

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem, System.Drawing
New-Item -ItemType Directory -Force $Out | Out-Null
$Out = (Resolve-Path $Out).Path

function Write-Zip([string]$path, [hashtable]$parts) {
    if (Test-Path $path) { Remove-Item $path }
    $zip = [System.IO.Compression.ZipFile]::Open($path, [System.IO.Compression.ZipArchiveMode]::Create)
    foreach ($name in $parts.Keys) {
        $entry = $zip.CreateEntry($name)
        $w = New-Object System.IO.StreamWriter($entry.Open(), (New-Object System.Text.UTF8Encoding($false)))
        $w.Write($parts[$name]); $w.Close()
    }
    $zip.Dispose()
}

# --- PDF: 3 pages of text, built by hand with a correct xref table --------
$pages = @("WinSearch sample PDF - page 1", "Page 2: zoom with the mouse wheel", "Page 3: use the arrows to change pages")
$objs = New-Object System.Collections.Generic.List[string]
$objs.Add("<< /Type /Catalog /Pages 2 0 R >>")
$kids = (0..2 | ForEach-Object { "$(4 + $_ * 2) 0 R" }) -join " "
$objs.Add("<< /Type /Pages /Kids [ $kids ] /Count 3 >>")
$objs.Add("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")
for ($i = 0; $i -lt 3; $i++) {
    $content = "BT /F1 28 Tf 60 720 Td ($($pages[$i])) Tj ET 0.2 0.5 0.9 rg 60 200 480 400 re f BT /F1 14 Tf 60 160 Td (Rendered by Windows.Data.Pdf in a helper process.) Tj ET"
    $objs.Add("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents $(5 + $i * 2) 0 R >>")
    $objs.Add("<< /Length $($content.Length) >>`nstream`n$content`nendstream")
}
$sb = New-Object System.Text.StringBuilder
[void]$sb.Append("%PDF-1.4`n")
$offsets = @()
for ($i = 0; $i -lt $objs.Count; $i++) {
    $offsets += $sb.Length
    [void]$sb.Append("$($i + 1) 0 obj`n$($objs[$i])`nendobj`n")
}
$xref = $sb.Length
[void]$sb.Append("xref`n0 $($objs.Count + 1)`n0000000000 65535 f `n")
foreach ($o in $offsets) { [void]$sb.Append(("{0:D10} 00000 n `n" -f $o)) }
[void]$sb.Append("trailer`n<< /Size $($objs.Count + 1) /Root 1 0 R >>`nstartxref`n$xref`n%%EOF`n")
[System.IO.File]::WriteAllText((Join-Path $Out "sample-3-pages.pdf"), $sb.ToString(), [System.Text.Encoding]::ASCII)

# --- Word ------------------------------------------------------------------
$ct = '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>'
$rels = '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>'
$doc = @'
<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>
<w:p><w:pPr><w:pStyle w:val="Title"/></w:pPr><w:r><w:t>Quarterly Project Report</w:t></w:r></w:p>
<w:p><w:r><w:t xml:space="preserve">This sample shows how WinSearch previews Word documents </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>without Microsoft Office</w:t></w:r><w:r><w:t xml:space="preserve">. Use Ctrl+mouse wheel or the + and - buttons to zoom.</w:t></w:r></w:p>
<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>Highlights</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Search latency under 10 ms for most queries</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Live index updates</w:t></w:r></w:p>
<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>Budget</w:t></w:r></w:p>
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>Item</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Cost</w:t></w:r></w:p></w:tc></w:tr>
<w:tr><w:tc><w:p><w:r><w:t>Hardware</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>1,200</w:t></w:r></w:p></w:tc></w:tr>
<w:tr><w:tc><w:p><w:r><w:t>Licenses</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>300</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
<w:p><w:r><w:rPr><w:i/></w:rPr><w:t>End of sample.</w:t></w:r></w:p>
</w:body></w:document>
'@
Write-Zip (Join-Path $Out "sample-report.docx") @{ "[Content_Types].xml" = $ct; "_rels/.rels" = $rels; "word/document.xml" = $doc }

# --- Excel -----------------------------------------------------------------
$xct = '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/></Types>'
$xrels = '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>'
$wb = '<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sales" sheetId="1" r:id="rId1"/><sheet name="Notes" sheetId="2" r:id="rId2"/></sheets></workbook>'
$wbrels = '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/></Relationships>'
$regions = @("North", "South", "East", "West")
$strings = @("Region", "Month", "Units", "Revenue") + $regions + @("Jan", "Feb", "Mar", "Remember: values are shown as stored.")
$sst = '<?xml version="1.0" encoding="UTF-8"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">' + (($strings | ForEach-Object { "<si><t>$_</t></si>" }) -join "") + '</sst>'
$rows = '<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c><c r="C1" t="s"><v>2</v></c><c r="D1" t="s"><v>3</v></c></row>'
$r = 2
for ($m = 0; $m -lt 3; $m++) {
    for ($g = 0; $g -lt 4; $g++) {
        $units = 100 + ($m * 37 + $g * 53) % 400
        $rows += "<row r=`"$r`"><c r=`"A$r`" t=`"s`"><v>$(4 + $g)</v></c><c r=`"B$r`" t=`"s`"><v>$(8 + $m)</v></c><c r=`"C$r`"><v>$units</v></c><c r=`"D$r`"><v>$($units * 19.99)</v></c></row>"
        $r++
    }
}
$sheet1 = '<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>' + $rows + '</sheetData></worksheet>'
$sheet2 = '<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="2"><c r="B2" t="s"><v>11</v></c></row></sheetData></worksheet>'
Write-Zip (Join-Path $Out "sample-sales.xlsx") @{ "[Content_Types].xml" = $xct; "_rels/.rels" = $xrels; "xl/workbook.xml" = $wb; "xl/_rels/workbook.xml.rels" = $wbrels; "xl/sharedStrings.xml" = $sst; "xl/worksheets/sheet1.xml" = $sheet1; "xl/worksheets/sheet2.xml" = $sheet2 }

# --- Large image (4000 x 2500) ---------------------------------------------
$bmp = New-Object System.Drawing.Bitmap 4000, 2500
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.SmoothingMode = "AntiAlias"
$rect = New-Object System.Drawing.Rectangle 0, 0, 4000, 2500
$brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush($rect, [System.Drawing.Color]::FromArgb(20, 40, 90), [System.Drawing.Color]::FromArgb(90, 160, 255), 35.0)
$g.FillRectangle($brush, $rect)
$pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(160, 255, 255, 255)), 3
for ($x = 0; $x -le 4000; $x += 100) { $g.DrawLine($pen, $x, 0, $x, 2500) }
for ($y = 0; $y -le 2500; $y += 100) { $g.DrawLine($pen, 0, $y, 4000, $y) }
$font = New-Object System.Drawing.Font "Segoe UI", 160, ([System.Drawing.FontStyle]::Bold)
$g.DrawString("Zoom me", $font, [System.Drawing.Brushes]::White, 1100, 1000)
$small = New-Object System.Drawing.Font "Segoe UI", 18
$g.DrawString("Fine print that is only readable when zoomed in: 4000 x 2500 pixels.", $small, [System.Drawing.Brushes]::White, 1400, 1450)
$g.Dispose()
$bmp.Save((Join-Path $Out "sample-large-image.png"), [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()

# --- Photo-sized JPEG (4000 x 3000, used for preview timing) ---------------
$bmp = New-Object System.Drawing.Bitmap 4000, 3000
$g = [System.Drawing.Graphics]::FromImage($bmp)
$rnd = New-Object System.Random 7
for ($i = 0; $i -lt 400; $i++) {
    $c = [System.Drawing.Color]::FromArgb($rnd.Next(256), $rnd.Next(256), $rnd.Next(256))
    $b = New-Object System.Drawing.SolidBrush $c
    $g.FillEllipse($b, $rnd.Next(4000), $rnd.Next(3000), $rnd.Next(50, 600), $rnd.Next(50, 600))
}
$g.Dispose()
$jpeg = [System.Drawing.Imaging.ImageCodecInfo]::GetImageEncoders() | Where-Object MimeType -eq "image/jpeg"
$ep = New-Object System.Drawing.Imaging.EncoderParameters 1
$ep.Param[0] = New-Object System.Drawing.Imaging.EncoderParameter([System.Drawing.Imaging.Encoder]::Quality, [long]90)
$bmp.Save((Join-Path $Out "sample-photo-12mp.jpg"), $jpeg, $ep)
$bmp.Dispose()

# --- Text --------------------------------------------------------------------
Set-Content -Encoding UTF8 (Join-Path $Out "sample-notes.md") "# Sample notes`n`nText and source files preview with line wrapping.`nCtrl+mouse wheel changes the text size.`n"
Get-ChildItem $Out | Select-Object Name, Length
