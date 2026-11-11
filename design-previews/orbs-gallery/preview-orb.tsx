// Review controls wrap the original component; upstream files stay untouched.
import React, { createContext, useContext } from 'react';
import { ThinkingOrb as OriginalOrb } from './upstream/packages/thinking-orbs/src/ThinkingOrb';
import type { ThinkingOrbProps } from './upstream/packages/thinking-orbs/src/types';
export type { OrbState, OrbSize, OrbTheme } from './upstream/packages/thinking-orbs/src/types';
export const PreviewControls = createContext({ paused: false });
export function ThinkingOrb(props: ThinkingOrbProps) {
  const { paused } = useContext(PreviewControls);
  return <OriginalOrb {...props} paused={paused || props.paused} />;
}
