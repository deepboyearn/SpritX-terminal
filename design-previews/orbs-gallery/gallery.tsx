import React, { useEffect, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { ThinkingOrb, PreviewControls, type OrbState, type OrbSize } from './preview-orb';
import { Examples } from './upstream/sites/orbs/components/Examples';
import { Playground } from './upstream/sites/orbs/components/Playground';

const STATES: { state: OrbState; label: string; description: string }[] = [
  { state: 'working', label: 'Working', description: 'Particles on tilted orbits' },
  { state: 'searching', label: 'Searching', description: 'A scanning line crosses a dotted globe' },
  { state: 'solving', label: 'Solving', description: 'Bands scramble and settle back into place' },
  { state: 'listening', label: 'Listening', description: 'A waveform moves through latitude rings' },
  { state: 'connecting', label: 'Connecting', description: 'A constellation builds connections' },
  { state: 'weaving', label: 'Weaving', description: 'Three strands weave around the sphere' },
  { state: 'composing', label: 'Composing', description: 'The “Thinking…” pill from your reference' },
  { state: 'breathing', label: 'Breathing', description: 'A ring slowly changes its shape' },
  { state: 'shaping', label: 'Shaping', description: 'Circle → triangle → square' },
];
const SIZES: OrbSize[] = [64, 32, 20];
const repo = 'https://github.com/Jakubantalik/Libraries.dev';
const revision = '387b049ca8fe97b5d72999b5bf61a19ab1ccff81';

function TerminalIcon() {
  return <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true"><rect x="3" y="4" width="18" height="16" rx="4"/><path d="m7 9 3 3-3 3m6 0h4"/></svg>;
}

function Gallery() {
  const initial = new URLSearchParams(location.search).get('state');
  const [selected, setSelected] = useState<OrbState>(STATES.some(s => s.state === initial) ? initial as OrbState : 'composing');
  const [theme, setTheme] = useState<'dark' | 'light'>('dark');
  const [paused, setPaused] = useState(false);
  const [speed, setSpeed] = useState(100);
  const [notice, setNotice] = useState('');
  const current = STATES.find(item => item.state === selected)!;
  useEffect(() => { document.documentElement.dataset.theme = theme; }, [theme]);
  const choose = (state: OrbState) => {
    setSelected(state);
    const url = new URL(location.href);
    url.searchParams.set('state', state);
    history.replaceState(null, '', url);
  };
  const copy = async () => {
    const selection = `${selected} · 20px · speed ${speed / 100} · ${theme} theme`;
    try { await navigator.clipboard.writeText(selection); setNotice('Selection copied'); }
    catch { setNotice(selection); }
  };
  return <PreviewControls.Provider value={{ paused }}>
    <main className={`review ${paused ? 'review-paused' : ''}`}>
      <header className="review-header">
        <div><a className="eyebrow" href={repo} target="_blank" rel="noreferrer">Libraries.dev / Original resources</a><h1>Thinking orbs</h1><p>Nine original animations. Choose one before the Rust implementation.</p></div>
        <div className="review-controls" aria-label="Gallery controls">
          <button aria-pressed={theme === 'dark'} onClick={() => setTheme('dark')}>Dark</button>
          <button aria-pressed={theme === 'light'} onClick={() => setTheme('light')}>Light</button>
          <button aria-pressed={paused} onClick={() => setPaused(!paused)}>{paused ? 'Play animations' : 'Pause animations'}</button>
        </div>
      </header>

      <section className="selection" aria-label="Selected component preview">
        <div className="selection-top"><div><span className="eyebrow">Selected component</span><h2 data-testid="selected-state">{current.label}</h2></div><button className="copy-selection" onClick={copy}>Copy selection</button></div>
        <div className="selection-stage">
          <div className="reference-stage"><span className="reference-pill"><ThinkingOrb state={selected} size={64} speed={speed / 100} style={{ width: 56, height: 56 }}/><span className="t-shimmer" data-text={selected === 'composing' ? 'Thinking….' : `${current.label}….`}>{selected === 'composing' ? 'Thinking….' : `${current.label}….`}</span></span><span className="stage-caption">Original 64px preset · displayed at 56px, as on the website</span></div>
          <div className="tab-stage"><span className="stage-caption">In the terminal tab · 20px</span><div className="terminal-mock"><div className="mock-tabs"><div className="mock-tab active"><TerminalIcon/><span>Terminal</span><ThinkingOrb state={selected} size={20} speed={speed / 100}/><span className="close">×</span></div><span className="mock-plus">+</span></div><div className="mock-transcript"><span>›</span> Working on your project…</div></div></div>
        </div>
        <div className="size-comparison">{SIZES.map(size => <div key={size}><ThinkingOrb state={selected} size={size} speed={speed / 100}/><span>{size}px {size === 32 ? '· interpolated' : '· tuned preset'}</span></div>)}</div>
        <div className="selection-bottom"><span>{current.description}</span><label>Speed <input aria-label="Preview speed" type="range" min="25" max="300" step="5" value={speed} onChange={e => setSpeed(Number(e.target.value))}/><output>{(speed / 100).toFixed(2)}×</output></label></div>
        <p className="copy-notice" role="status">{notice}</p>
      </section>

      <section aria-label="All nine original orb animations" className="catalog-section">
        <div className="section-heading"><h2>All nine states</h2><p>Click a component to inspect it above.</p></div>
        <div className="orb-catalog">{STATES.map((item, index) => <button key={item.state} className="orb-card" aria-pressed={selected === item.state} data-state={item.state} onClick={() => choose(item.state)}><span className="card-top"><span>{String(index + 1).padStart(2, '0')}</span><span>{selected === item.state ? 'Selected' : 'Preview'}</span></span><span className="card-orb"><ThinkingOrb state={item.state} size={64} speed={speed / 100}/></span><span className="card-title">{item.label}</span><span className="card-description">{item.description}</span><span className="card-inline"><ThinkingOrb state={item.state} size={20} speed={speed / 100}/><span>20px tab version</span></span></button>)}</div>
      </section>

      <section className="original-section" aria-label="Original website pill and chip examples"><div className="section-heading"><h2>Original pill &amp; chip UI</h2><p>The repository’s original Examples component, with its own labels and layouts.</p></div><Examples speed={speed / 100}/></section>
      <section className="original-section" aria-label="Original website interactive playground"><div className="section-heading"><h2>Original controls</h2><p>State, size, speed, play/pause and code copy from the repository.</p></div><Playground speed={speed} onSpeedChange={setSpeed}/></section>

      <footer className="resource-footer"><div><h2>Source resources</h2><p>Original source is kept unchanged under <code>upstream/</code>. Browser preview only; no new terminal integration.</p></div><div className="resource-links"><a href="upstream/packages/thinking-orbs/README.md">Component documentation</a><a href="upstream/packages/thinking-orbs/src/ThinkingOrb.tsx">ThinkingOrb source</a><a href="upstream/packages/thinking-orbs/src/presets.ts">All state &amp; size presets</a><a href="upstream/sites/orbs/styles.css">Pill and shimmer CSS</a><a href="RESOURCE-MAP.md">UI classes &amp; resource map</a><a href="resources.json">Verified file manifest</a><a href="upstream/packages/thinking-orbs/LICENSE">MIT license</a></div><p className="revision">Jakub Antalik · revision <a href={`${repo}/tree/${revision}`}>{revision.slice(0, 12)}</a></p></footer>
    </main>
  </PreviewControls.Provider>;
}
createRoot(document.getElementById('root')!).render(<Gallery/>);
