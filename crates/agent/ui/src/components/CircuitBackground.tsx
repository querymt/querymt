export function CircuitBackground({ className = '' }: { className?: string }) {
  return (
    <svg
      className={`circuit-background ${className}`}
      xmlns="http://www.w3.org/2000/svg"
      width="2400"
      height="1600"
      viewBox="0 0 2400 1600"
      preserveAspectRatio="xMidYMid slice"
      style={{
        position: 'absolute',
        top: 0,
        left: 0,
        width: '100%',
        height: '100%',
        pointerEvents: 'none',
      }}
    >
      {/* Group 1 - Primary paths */}
      <g className="circuit-path path-1">
        <path d="M0 239h24l32 32v124l12 12v4" fill="none" stroke="#00fff9" strokeWidth="1.6" />
        <path d="M0 247h20l29 29v124l3 3v8" fill="none" stroke="#00fff9" strokeWidth="1.6" />
        <path d="M0 271h7l16 16v121l-11 11v77l-13 13" fill="none" stroke="#00fff9" strokeWidth="1.6" />
        <path d="M-1 521l21-21v-76l12-12V284l-21-21H0" fill="none" stroke="#00fff9" strokeWidth="1.6" />
        <path d="M-1 556v-24l29-29v-75l12-12V279l-24-24H0m300 221h60l16-16h95M0 156h76l44 44v116l36 36h20l32 32v16l-12 12v40l32 32h24m48 0h91l4-4h69l4-4h8l44 44h36" fill="none" stroke="#00fff9" strokeWidth="1.6" />
        <path d="M300 468h56l16-16h4l4-4M0 132h88l56 56v108l80 80v32l-12 12v24l24 24h16" fill="none" stroke="#00fff9" strokeWidth="1.6" />
      </g>

      {/* Group 2 - Secondary paths with delay */}
      <g className="circuit-path path-2">
        <path d="M600 0v116l44 44h112l56 56v124l12 12v8m0 128l-12 12v36l32 32h24m48 0h88l44 44v116l36 36h20l32 32v16l-12 12v40l32 32h24" fill="none" stroke="#00fff9" strokeWidth="1.6" />
        <path d="M900 200h56l16-16h100m-500 0h76l44 44v116l36 36h20l32 32v16l-12 12v40l32 32h24" fill="none" stroke="#00fff9" strokeWidth="1.6" />
        <path d="M1200 400v116l44 44h112l56 56v124l12 12v8" fill="none" stroke="#00fff9" strokeWidth="1.6" />
        <path d="M1500 600h56l16-16h100M1200 300h88l56 56v108l80 80v32l-12 12v24l24 24h16" fill="none" stroke="#00fff9" strokeWidth="1.6" />
      </g>

      {/* Group 3 - Tertiary paths with more delay */}
      <g className="circuit-path path-3">
        <path d="M1800 100v116l44 44h112l56 56v124l12 12v8m0 128l-12 12v36l32 32h24" fill="none" stroke="#00fff9" strokeWidth="1.6" />
        <path d="M2100 300h56l16-16h100m-300 200v116l44 44h112" fill="none" stroke="#00fff9" strokeWidth="1.6" />
        <path d="M2400 800h-88l-56-56v-108l-80-80v-32l12-12v-24l-24-24h-16" fill="none" stroke="#00fff9" strokeWidth="1.6" />
        <path d="M1800 1200v-116l-44-44h-112l-56-56v-124l-12-12v-8" fill="none" stroke="#00fff9" strokeWidth="1.6" />
      </g>

      {/* Group 4 - Quaternary paths with max delay */}
      <g className="circuit-path path-4">
        <path d="M300 1400v-116l-44-44h-112l-56-56v-124l-12-12v-8m0-128l12-12v-36l-32-32h-24" fill="none" stroke="#00fff9" strokeWidth="1.6" />
        <path d="M600 1200h-56l-16 16h-100m500 0h-76l-44-44v-116l-36-36h-20l-32-32v-16l12-12v-40l-32-32h-24" fill="none" stroke="#00fff9" strokeWidth="1.6" />
        <path d="M1200 1000v-116l-44-44h-112l-56-56v-124l-12-12v-8" fill="none" stroke="#00fff9" strokeWidth="1.6" />
        <path d="M1500 800h-56l-16 16h-100M1200 1100h-88l-56-56v-108l-80-80v-32l12-12v-24l-24-24h-16" fill="none" stroke="#00fff9" strokeWidth="1.6" />
        <path d="M2000 1500v-116l-44-44h-112l-56-56v-124l-12-12v-8m0-128l12-12v-36l-32-32h-24" fill="none" stroke="#00fff9" strokeWidth="1.6" />
      </g>
    </svg>
  );
}
