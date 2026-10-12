// FlowLite branding: wordmark with a microphone glyph (Handy's logo is not open source).
const HandyTextLogo = ({
  width,
  height,
  className,
}: {
  width?: number;
  height?: number;
  className?: string;
}) => (
  <svg
    width={width}
    height={height}
    className={className}
    viewBox="0 0 300 80"
    xmlns="http://www.w3.org/2000/svg"
  >
    <rect x="0" y="4" width="72" height="72" rx="16" fill="#3559E0" />
    <path
      transform="translate(6 10) scale(2.5)"
      fill="#F5F5F5"
      d="M12 14a3 3 0 0 0 3-3V5a3 3 0 0 0-6 0v6a3 3 0 0 0 3 3zm5-3a5 5 0 0 1-10 0H5a7 7 0 0 0 6 6.92V21h2v-3.08A7 7 0 0 0 19 11z"
    />
    <text
      x="88"
      y="58"
      className="fill-text"
      fontFamily="system-ui, sans-serif"
      fontSize="48"
      fontWeight="700"
    >
      {"FlowLite" /* brand name, not translated */}
    </text>
  </svg>
);

export default HandyTextLogo;
