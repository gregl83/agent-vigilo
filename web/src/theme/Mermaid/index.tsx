import OriginalMermaid from '@theme-original/Mermaid';
import React, {type ReactNode, useEffect, useState} from 'react';
import type {Props} from '@theme/Mermaid';

let iconRegistrationPromise: Promise<void> | undefined;

function registerFontAwesomeIcons(): Promise<void> {
  iconRegistrationPromise ??= Promise.all([
    import('mermaid'),
    import('@iconify-json/fa6-solid/icons.json'),
  ]).then(([mermaidModule, fa6SolidIcons]) => {
    mermaidModule.default.registerIconPacks([
      {name: 'fa', icons: fa6SolidIcons.default},
      {name: 'fas', icons: fa6SolidIcons.default},
    ]);
  });

  return iconRegistrationPromise;
}

export default function Mermaid(props: Props): ReactNode {
  const [iconsReady, setIconsReady] = useState(false);

  useEffect(() => {
    let mounted = true;

    registerFontAwesomeIcons()
      .catch((error: unknown) => {
        console.error('Failed to register Mermaid Font Awesome icons', error);
      })
      .finally(() => {
        if (mounted) {
          setIconsReady(true);
        }
      });

    return () => {
      mounted = false;
    };
  }, []);

  if (!iconsReady) {
    return null;
  }

  return <OriginalMermaid {...props} />;
}
