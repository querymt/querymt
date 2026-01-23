import { useEffect, useRef } from 'react';
// @ts-ignore - No types available for splitting
import Splitting from 'splitting';

interface GlitchTextProps {
  text: string;
  variant?: '0' | '3' | '5' | '8' | '12';
  className?: string;
  hoverOnly?: boolean;
}

export function GlitchText({
  text,
  variant = '3',
  hoverOnly = false,
  className = '',
}: GlitchTextProps) {
  const textRef = useRef<HTMLSpanElement>(null);
  const shouldHover = hoverOnly && variant === '12';
  const shouldHoverJump = hoverOnly && variant === '0';

  useEffect(() => {
    if (textRef.current) {
      // Run Splitting.js to split text into individual characters
      Splitting({ target: textRef.current });
      if (variant === '0') {
        const chars = Array.from(textRef.current.querySelectorAll<HTMLElement>('[data-char]'));
        const activeCount = Math.max(1, Math.ceil(chars.length * 0.35));
        chars.forEach((char) => {
          char.classList.remove('glitch-0-active');
        });
        const shuffled = chars
          .map((char) => ({ char, sort: Math.random() }))
          .sort((a, b) => a.sort - b.sort)
          .slice(0, activeCount)
          .map(({ char }) => char);
        shuffled.forEach((char) => {
          char.classList.add('glitch-0-active');
          const rand = (min: number, max: number) => Math.random() * (max - min) + min;
          char.style.setProperty('--x-1', rand(-50, 50).toFixed(1));
          char.style.setProperty('--y-1', rand(-50, 50).toFixed(1));
          char.style.setProperty('--x-2', rand(-50, 50).toFixed(1));
          char.style.setProperty('--y-2', rand(-50, 50).toFixed(1));
          char.style.setProperty('--scale-1', rand(0.7, 1.4).toFixed(2));
          char.style.setProperty('--scale-2', rand(0.7, 1.4).toFixed(2));
          char.style.setProperty('--speed', rand(0.8, 1.6).toFixed(2));
        });
      }
    }
  }, [text, variant]);

  return (
    <span
      ref={textRef}
      data-splitting=""
      className={`glitch ${shouldHover || shouldHoverJump ? '' : `glitch--${variant}`} ${className}`}
      onMouseEnter={() => {
        if (shouldHover) {
          textRef.current?.classList.add('glitch--12');
        }
        if (shouldHoverJump) {
          textRef.current?.classList.add('glitch--0');
        }
      }}
      onMouseLeave={() => {
        if (shouldHover) {
          textRef.current?.classList.remove('glitch--12');
        }
        if (shouldHoverJump) {
          textRef.current?.classList.remove('glitch--0');
        }
      }}
    >
      {text}
    </span>
  );
}
