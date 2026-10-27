// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
import {useEffect, useRef, useState} from 'react';
import type {ReactNode} from 'react';
import clsx from 'clsx';

type RevealProps = {
  children: ReactNode;
  className?: string;
  /** Stagger delay in ms, for revealing a sequence of siblings. */
  delay?: number;
};

/**
 * Fades/slides a section in once it scrolls into view. Falls back to
 * always-visible (no animation) when IntersectionObserver isn't available
 * (SSR/build) or the user prefers reduced motion — handled in CSS via
 * `.zv-reveal` (see src/css/custom.css).
 */
export default function Reveal({children, className, delay = 0}: RevealProps) {
  const ref = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(false);

  useEffect(() => {
    const node = ref.current;
    if (!node || typeof IntersectionObserver === 'undefined') {
      setVisible(true);
      return;
    }
    const observer = new IntersectionObserver(
      ([entry]) => {
        if (entry.isIntersecting) {
          setVisible(true);
          observer.disconnect();
        }
      },
      // A tall wrapper (the feature bands) can never show 15% of itself in a short window, so
      // trigger as soon as any of it enters instead of by area.
      {threshold: 0, rootMargin: '0px 0px -8% 0px'},
    );
    observer.observe(node);
    return () => observer.disconnect();
  }, []);

  return (
    <div
      ref={ref}
      className={clsx('zv-reveal', visible && 'zv-reveal--visible', className)}
      style={delay ? {transitionDelay: `${delay}ms`} : undefined}>
      {children}
    </div>
  );
}
