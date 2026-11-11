`orb-composing-20.json` contains the four `composing`, size-20 cases from
Thinking Orbs' upstream `spec/orbs-golden.json`, revision
`387b049ca8fe97b5d72999b5bf61a19ab1ccff81`:

https://github.com/Jakubantalik/Libraries.dev/blob/387b049ca8fe97b5d72999b5bf61a19ab1ccff81/packages/thinking-orbs/spec/orbs-golden.json

Each dot is six consecutive values: x, y, z, radius, ink-white, alpha.
Times are engine time, before the renderer applies the composing preset's
speed of 3.12 and the approved user multiplier of 2.05 (6.396 engine seconds
per wall-clock second, matching the browser gallery).
The native Rust geometry is compared against these independent reference
vectors with the upstream tolerance of 0.0001. Rendering uses the original
quantized grayscale ink and alpha in both light and dark themes. Geometry,
speed selection, color mapping and bounds are checked independently.

Upstream copyright and MIT license are in
`assets/licenses/Thinking-Orbs-MIT.txt`. No upstream JavaScript or TypeScript
runtime is included.
