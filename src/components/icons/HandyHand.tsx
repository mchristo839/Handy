// FlowLite branding: parakeet head (Handy's hand logo is not open source).
const HandyHand = ({
  width,
  height,
}: {
  width?: number | string;
  height?: number | string;
}) => (
  <svg
    width={width || 24}
    height={height || 24}
    viewBox="0 0 100 100"
    className="fill-text"
    xmlns="http://www.w3.org/2000/svg"
  >
    <path
      fillRule="evenodd"
      d="M10,50 a32,32 0 1,0 64,0 a32,32 0 1,0 -64,0 Z M58,42 a6,6 0 1,0 12,0 a6,6 0 1,0 -12,0 Z"
    />
    <path d="M70,50 C77,48 88,52 89,61 C90,68 86,74 81,75 C82,69 80,65 75,64 C71,63 68,58 70,50 Z" />
  </svg>
);

export default HandyHand;
