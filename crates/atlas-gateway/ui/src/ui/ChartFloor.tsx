// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
/** Signature 01 — drifting bathymetric chart floor behind the shell. */

export function ChartFloor() {
  return (
    <>
      <div className="at-chart-floor" aria-hidden>
        <svg viewBox="0 0 1600 1000" preserveAspectRatio="xMidYMid slice">
          <g className="grid">
            <path d="M0 200H1600M0 400H1600M0 600H1600M0 800H1600" />
            <path d="M200 0V1000M400 0V1000M600 0V1000M800 0V1000M1000 0V1000M1200 0V1000M1400 0V1000" />
          </g>
          <g className="iso">
            <ellipse cx="330" cy="300" rx="90" ry="58" className="iso major" />
            <ellipse cx="330" cy="300" rx="160" ry="104" />
            <ellipse cx="335" cy="302" rx="238" ry="152" className="iso major" />
            <ellipse cx="340" cy="306" rx="322" ry="204" />
            <ellipse cx="346" cy="310" rx="412" ry="262" className="iso major" />
            <ellipse cx="352" cy="316" rx="510" ry="326" />
            <ellipse cx="1240" cy="720" rx="70" ry="46" className="iso major" />
            <ellipse cx="1240" cy="720" rx="138" ry="92" />
            <ellipse cx="1236" cy="722" rx="214" ry="142" className="iso major" />
            <ellipse cx="1232" cy="726" rx="298" ry="196" />
            <ellipse cx="1226" cy="730" rx="392" ry="256" className="iso major" />
            <path d="M0 640 C 200 600 340 700 520 664 C 700 628 840 720 1010 690 C 1180 660 1400 740 1600 706" />
            <path
              d="M0 700 C 210 662 350 762 530 726 C 710 690 850 782 1020 752 C 1190 722 1410 802 1600 768"
              className="iso major"
            />
            <path d="M0 120 C 240 90 400 178 600 146 C 800 114 940 196 1120 168 C 1300 140 1450 210 1600 186" />
          </g>
        </svg>
      </div>
      <div className="at-vignette" aria-hidden />
    </>
  );
}
