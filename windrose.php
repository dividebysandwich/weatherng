<?php
putenv('GDFONTPATH=' . realpath('.'));

function normalize($angle)
{
    while ($angle >= 360) $angle -= 360;
    while ($angle < 0) $angle += 360;
    return $angle;
}

header("Content-Type: image/png");
header("Expires: Mon, 26 Jul 1997 05:00:00 GMT");    // Date in the past
header("Last-Modified: " . gmdate("D, d M Y H:i:s") . " GMT");
header("Cache-Control: no-cache, must-revalidate");  // HTTP/1.1
header("Pragma: no-cache");                          // HTTP/1.0

date_default_timezone_set('Europe/Vienna');

// --- 1. Fetch & Parse Historical Data ---
$jsonUrl = 'https://hoxdna.org/queryWeather';
$jsonData = @file_get_contents($jsonUrl);
$data = json_decode($jsonData, true);

if (!$data) {
    // Fallback for connection failure
    $data = [
        'curwindspeed' => 0, 'curwindgust' => 0, 'curwinddir' => 0,
        'windspeeds' => [], 'windgusts' => [], 'winddirs' => [],
        'temperature' => [], 'temperatureindoor' => [],
        'curtemperature' => 0, 'currain' => 0, 'curhumidity' => 0, 'time' => date('c', 0)
    ];
}

$windspeedkmh = round((float)$data['curwindspeed'], 1);
$gustkmh = round((float)$data['curwindgust'], 1);
$winddirection = round((float)$data['curwinddir']);

$windspeeds = $data['windspeeds'] ?: [];
$lastgusts = $data['windgusts'] ?: [];
$winddirs = $data['winddirs'] ?: [];
$temperature = $data['curtemperature'];
$indoortemperature = !empty($data['temperatureindoor']) ? round(end($data['temperatureindoor']), 1) : 0;
$rain = $data['currain'];
$humid = $data['curhumidity'];
$lastupdate = strtotime($data['time']);

// Align historical arrays and capture point count
$limit = min(count($windspeeds), count($lastgusts));
$plotSpeeds = array_slice($windspeeds, -$limit);
$plotGusts  = array_slice($lastgusts,  -$limit);
$numPoints  = count($plotGusts);

// --- 2. Fetch & Parse Forecast Data ---
// Expand hourly forecast to the same 2-min resolution as the historical data so
// both halves of the graph have identical point counts (true 50/50 split).
$forecastGusts = [];
$forecastJson  = @file_get_contents('https://hoxdna.org/forecast');
if ($forecastJson) {
    $forecastData = json_decode($forecastJson, true);
    if ($forecastData && isset($forecastData['hourly'])) {
        $hourly = $forecastData['hourly'];
        // Build map: UTC-hour-floor-timestamp → gust value
        // Open-Meteo with timezone=UTC returns "2024-01-15T14:00" (no Z suffix)
        $gustByHour = [];
        foreach ($hourly['time'] as $i => $timeStr) {
            $t = strtotime(str_replace('T', ' ', $timeStr) . ' UTC');
            if ($t !== false) {
                $hourFloor = $t - ($t % 3600);
                $gustByHour[$hourFloor] = (float)$hourly['wind_gusts_10m'][$i];
            }
        }
        // Step forward numPoints × 2 min from latest historical timestamp
        $latestTs    = strtotime($data['time']); // RFC3339 from Rust, already UTC-aware
        $intervalSec = 120; // 2 minutes, same as historical resolution
        for ($i = 0; $i < $numPoints; $i++) {
            $futureTs  = $latestTs + ($i + 1) * $intervalSec;
            $hourFloor = $futureTs - ($futureTs % 3600);
            $forecastGusts[] = isset($gustByHour[$hourFloor]) ? $gustByHour[$hourFloor] : 0.0;
        }
    }
}
// Pad with zeros so the array is always numPoints long (covers fetch failure)
while (count($forecastGusts) < $numPoints) $forecastGusts[] = 0.0;

// --- 3. Canvas Setup (Oversampled for Anti-Aliasing) ---
$finalimagewidth = 280;
$finalimageheight = 310;
$sizefactor = 4; // Render at 4x scale, then resample down for smooth lines

$imagewidth = $finalimagewidth * $sizefactor;
$imageheight = $finalimageheight * $sizefactor;
$image = imagecreatetruecolor($imagewidth, $imageheight);

// --- 4. Modern Color Palette ---
$bg = imagecolorallocate($image, 24, 24, 28);
$dialSubtle      = imagecolorallocatealpha($image, 100, 100, 115, 60);
$dialVariability = imagecolorallocatealpha($image, 0, 173, 181, 105);

$graphGustFill  = imagecolorallocatealpha($image, 255, 46, 99, 90);
$graphGustLine  = imagecolorallocate($image, 255, 46, 99);
$graphSpeedFill = imagecolorallocatealpha($image, 0, 173, 181, 70);
$graphSpeedLine = imagecolorallocate($image, 0, 173, 181);

// Forecast: same teal as wind speed at 50 % alpha, diagonal hatching
$fcstFill   = imagecolorallocatealpha($image, 0, 173, 181, 64);  // 50 % transparent teal
$fcstLine   = imagecolorallocate($image, 0, 173, 181);           // solid teal dashed border
$fcstStripe = imagecolorallocatealpha($image, 0, 173, 181, 115);

$gridHour  = imagecolorallocatealpha($image, 180, 180, 200, 88);
$textWhite = imagecolorallocate($image, 240, 240, 240);
$textGray  = imagecolorallocate($image, 150, 150, 160);

imagefill($image, 0, 0, $bg);

// --- 5. Wind Rose & Dial ---
$cx = 140 * $sizefactor;
$cy = 115 * $sizefactor;
$radius = 160 * $sizefactor;

imagesetthickness($image, 2 * $sizefactor);
imagearc($image, $cx, $cy, $radius, $radius, 0, 360, $dialSubtle);
imagearc($image, $cx, $cy, $radius * 0.7, $radius * 0.7, 0, 360, $dialSubtle);

if (!empty($winddirs)) {
    imagesetthickness($image, 6 * $sizefactor);
    $recentWindDirs = array_slice($winddirs, -30);
    foreach ($recentWindDirs as $dir) {
        $startAngle = $dir - 5 - 90;
        $endAngle   = $dir + 5 - 90;
        imagearc($image, $cx, $cy, $radius - (4*$sizefactor), $radius - (4*$sizefactor), $startAngle, $endAngle, $dialVariability);
    }
}

// --- 6. Dynamic Wind Arrow ---
$arrowheight = 135 * $sizefactor;

if ($gustkmh < 3) {
    $thickness = 10;
    $arrowColor = imagecolorallocate($image, 150, 150, 150);
} else if ($gustkmh <= 10) {
    $thickness = 15;
    $arrowColor = imagecolorallocate($image, 80, 220, 100);
} else if ($gustkmh < 20) {
    $thickness = 20;
    $arrowColor = imagecolorallocate($image, 170, 220, 50);
} else if ($gustkmh < 25) {
    $thickness = 25;
    $arrowColor = imagecolorallocate($image, 240, 180, 50);
} else if ($gustkmh < 30) {
    $thickness = 30;
    $arrowColor = imagecolorallocate($image, 255, 120, 50);
} else {
    $thickness = 35;
    $arrowColor = imagecolorallocate($image, 255, 50, 50);
}

$coord = [
    [0, -$arrowheight/2],
    [-$thickness * $sizefactor, $arrowheight/2],
    [0, ($arrowheight/2) - (10 + ($thickness/4)) * $sizefactor],
    [$thickness * $sizefactor, $arrowheight/2]
];

$angle = deg2rad($winddirection);
$flat_coords = [];
foreach ($coord as $pt) {
    $x = $pt[0]; $y = $pt[1];
    $flat_coords[] = ($x * cos($angle) - $y * sin($angle)) + $cx;
    $flat_coords[] = ($x * sin($angle) + $y * cos($angle)) + $cy;
}
imagefilledpolygon($image, $flat_coords, count($coord), $arrowColor);
imagesetthickness($image, 2 * $sizefactor);
imagepolygon($image, $flat_coords, count($coord), $textWhite);

// --- 7. Helper Functions ---

// Solid filled area graph (historical data)
function drawAreaGraph($img, $data, $gx, $gy, $gw, $gh, $max, $lineColor, $fillColor, $size) {
    $count = count($data);
    if ($count < 2) return;
    $stepX = $gw / ($count - 1);
    $poly  = [$gx, $gy + $gh];
    $prevX = $gx;
    $prevY = $gy + $gh - (($data[0] / $max) * $gh);
    $poly[] = $prevX; $poly[] = $prevY;
    imagesetthickness($img, 2 * $size);
    for ($i = 1; $i < $count; $i++) {
        $x = $gx + ($i * $stepX);
        $y = $gy + $gh - (($data[$i] / $max) * $gh);
        $poly[] = $x; $poly[] = $y;
        imageline($img, $prevX, $prevY, $x, $y, $lineColor);
        $prevX = $x; $prevY = $y;
    }
    $poly[] = $prevX; $poly[] = $gy + $gh;
    imagefilledpolygon($img, $poly, count($poly)/2, $fillColor);
}

// Striped + dashed-outline area graph (forecast data)
function drawForecastGraph($img, $data, $gx, $gy, $gw, $gh, $max, $lineColor, $fillColor, $stripeColor, $size) {
    $count = count($data);
    if ($count < 2) return;
    $stepX = $gw / ($count - 1);

    // Diagonal stripe hatching only — no solid fill.
    // Each unclipped line runs (gx+offset, gy+gh) → (gx+offset+gh, gy), slope dy/dx = -1.
    // Clip to x ∈ [gx, gx+gw] so stripes never cross the "now" divider on the left
    // or bleed past the right edge.
    //   y at x=gx      → (gy+gh) + offset
    //   y at x=gx+gw   → (gy+gh) − gw + offset
    $stripeSpacing = 8 * $size;
    imagesetthickness($img, 2 * $size);
    for ($offset = -$gh; $offset <= $gw + $gh; $offset += $stripeSpacing) {
        $x1 = $gx + $offset;
        $y1 = $gy + $gh;
        $x2 = $gx + $offset + $gh;
        $y2 = $gy;
        if ($x1 < $gx) {               // clip left edge
            $y1 = (int)($gy + $gh + $offset);
            $x1 = $gx;
        }
        if ($x2 > $gx + $gw) {         // clip right edge
            $y2 = (int)($gy + $gh - $gw + $offset);
            $x2 = $gx + $gw;
        }
        if ($x1 < $x2) {
            imageline($img, $x1, $y1, $x2, $y2, $stripeColor);
        }
    }

    // Dashed top border
    $dash = [];
    for ($i = 0; $i < 4 * $size; $i++) $dash[] = $lineColor;
    for ($i = 0; $i < 4 * $size; $i++) $dash[] = IMG_COLOR_TRANSPARENT;
    imagesetstyle($img, $dash);
    imagesetthickness($img, 2 * $size);
    $prevX = $gx;
    $prevY = $gy + $gh - (($data[0] / $max) * $gh);
    for ($i = 1; $i < $count; $i++) {
        $x = $gx + ($i * $stepX);
        $y = $gy + $gh - (($data[$i] / $max) * $gh);
        imageline($img, $prevX, $prevY, $x, $y, IMG_COLOR_STYLED);
        $prevX = $x; $prevY = $y;
    }
}

// --- 8. Histogram Graph (Bottom Space) ---
$graphX = 10 * $sizefactor;
$graphY = 220 * $sizefactor;
$graphW = 260 * $sizefactor;
$graphH = 50 * $sizefactor;
$halfW  = (int)($graphW / 2); // Historical occupies left half, forecast right half

// Unified Y-axis scale covering both historical and forecast peaks
$maxgraphspeed = 15;
foreach ($plotSpeeds    as $s) if ($s > $maxgraphspeed) $maxgraphspeed = $s;
foreach ($plotGusts     as $g) if ($g > $maxgraphspeed) $maxgraphspeed = $g;
foreach ($forecastGusts as $g) if ($g > $maxgraphspeed) $maxgraphspeed = $g;

// Horizontal dashed grid lines (full width)
$dashH = [];
for ($i = 0; $i < 4 * $sizefactor; $i++) $dashH[] = $dialSubtle;
for ($i = 0; $i < 6 * $sizefactor; $i++) $dashH[] = IMG_COLOR_TRANSPARENT;
imagesetstyle($image, $dashH);
imagesetthickness($image, 1 * $sizefactor);
for ($i = 0; $i <= 2; $i++) {
    $yLine = $graphY + ($graphH / 2 * $i);
    imageline($image, $graphX, $yLine, $graphX + $graphW, $yLine, IMG_COLOR_STYLED);
}

// Area graphs
drawAreaGraph($image, $plotGusts,      $graphX,          $graphY, $halfW, $graphH, $maxgraphspeed, $graphGustLine,  $graphGustFill,  $sizefactor);
drawAreaGraph($image, $plotSpeeds,     $graphX,          $graphY, $halfW, $graphH, $maxgraphspeed, $graphSpeedLine, $graphSpeedFill, $sizefactor);
drawForecastGraph($image, $forecastGusts, $graphX + $halfW, $graphY, $halfW, $graphH, $maxgraphspeed, $fcstLine, $fcstFill, $fcstStripe, $sizefactor);

// Vertical hourly lines drawn on top of the area fills.
// Historical half: lines every 30 data points counting back from "now" (right edge).
// Forecast half:   lines every 30 slots counting forward from "now" (left edge).
// 30 points × 2 min = 1 hour on both sides.
$pxPerPoint = ($numPoints > 1) ? $halfW / ($numPoints - 1) : 1.0;
$pxPerHour  = 30.0 * $pxPerPoint;
$dashV = [];
for ($i = 0; $i < 3 * $sizefactor; $i++) $dashV[] = $gridHour;
for ($i = 0; $i < 5 * $sizefactor; $i++) $dashV[] = IMG_COLOR_TRANSPARENT;
imagesetstyle($image, $dashV);
imagesetthickness($image, 1 * $sizefactor);
for ($h = 1; $h * $pxPerHour < $halfW; $h++) {
    // Historical: count back from the "now" divider
    $xHist = (int)($graphX + $halfW - $h * $pxPerHour);
    imageline($image, $xHist, $graphY, $xHist, $graphY + $graphH, IMG_COLOR_STYLED);
    // Forecast: count forward from the "now" divider
    $xFcst = (int)($graphX + $halfW + $h * $pxPerHour);
    imageline($image, $xFcst, $graphY, $xFcst, $graphY + $graphH, IMG_COLOR_STYLED);
}

// "Now" centre divider (solid)
imagesetthickness($image, 2 * $sizefactor);
imageline($image, $graphX + $halfW, $graphY, $graphX + $halfW, $graphY + $graphH, $textGray);

// Baseline
imageline($image, $graphX, $graphY + $graphH, $graphX + $graphW, $graphY + $graphH, $dialSubtle);


// --- 9. Resample and Render Text (1x scale) ---
$finalimage = imagecreatetruecolor($finalimagewidth, $finalimageheight);
imagecopyresampled($finalimage, $image, 0, 0, 0, 0, $finalimagewidth, $finalimageheight, $imagewidth, $imageheight);

$tWhite = imagecolorallocate($finalimage, 240, 240, 240);
$tGray  = imagecolorallocate($finalimage, 160, 160, 170);
$tCyan  = imagecolorallocate($finalimage, 0, 212, 255);
$tPink  = imagecolorallocate($finalimage, 255, 46, 99);
$tRed   = imagecolorallocate($finalimage, 255, 50, 50);

// Top Information
imagestring($finalimage, 4, 10, 10, "GUST", $tGray);
imagestring($finalimage, 5, 10, 25, "{$gustkmh} km/h", ($gustkmh < 20 ? $tWhite : ($gustkmh < 30 ? $tCyan : $tPink)));

$tempStr = "{$temperature} C";
imagestring($finalimage, 4, 270 - (imagefontwidth(4) * strlen($tempStr)), 10, "TEMP", $tGray);
imagestring($finalimage, 5, 270 - (imagefontwidth(5) * strlen($tempStr)), 25, $tempStr, $tWhite);

// Compass directions
imagestring($finalimage, 4, 136, 18, "N", $tGray);
imagestring($finalimage, 4, 136, 202, "S", $tGray);
imagestring($finalimage, 4, 42, 108, "W", $tGray);
imagestring($finalimage, 4, 230, 108, "E", $tGray);

// Error Handling / Stale Data
if (time() - $lastupdate > 600) {
    imagefill($finalimage, 0, 0, imagecolorallocatealpha($finalimage, 20, 0, 0, 80));
    imagestring($finalimage, 4, 140 - (imagefontwidth(4)*16/2), 140, "NO CONNECTION!!", $tRed);
    imagestring($finalimage, 2, 140 - (imagefontwidth(2)*28/2), 160, "Offline since: " . date("H:i", $lastupdate), $tWhite);
}

// Bottom Stats Grid
$by = 280;
imagestring($finalimage, 2, 10, $by, "Avg Wind:", $tGray);
imagestring($finalimage, 2, 70, $by, "{$windspeedkmh} km/h", $tCyan);

imagestring($finalimage, 2, 150, $by, "Rain:", $tGray);
imagestring($finalimage, 2, 190, $by, "{$rain} mm", $tWhite);

imagestring($finalimage, 2, 10, $by+15, "Indoor:", $tGray);
imagestring($finalimage, 2, 70, $by+15, "{$indoortemperature} C", $tWhite);

imagestring($finalimage, 2, 150, $by+15, "Humid:", $tGray);
imagestring($finalimage, 2, 195, $by+15, "{$humid} %", $tWhite);

// Render and Cleanup
imagepng($finalimage);
imagedestroy($image);
imagedestroy($finalimage);
?>
