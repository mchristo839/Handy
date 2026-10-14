// FlowLite branding: wordmark with the parakeet app icon (Handy's logo is not open source).
import parakeet from "../../../src-tauri/icons/128x128@2x.png";

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
    <image href={parakeet} x="0" y="4" width="72" height="72" />
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
