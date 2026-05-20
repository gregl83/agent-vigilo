import ErrorBoundary from '@docusaurus/ErrorBoundary';
import {useLocation} from '@docusaurus/router';
import {ErrorBoundaryErrorMessageFallback} from '@docusaurus/theme-common';
import {
  MermaidContainerClassName,
  useMermaidConfig,
  useMermaidRenderResult,
} from '@docusaurus/theme-mermaid/client';
import type {Props} from '@theme/Mermaid';
import type {RenderResult} from 'mermaid';
import React, {
  type ReactNode,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react';

import DiagramViewport from '@site/src/components/DiagramViewport';

let iconRegistrationPromise: Promise<void> | undefined;

const VIEWPORT_DOC_PATH_PREFIXES = [
  '/docs/architecture/flows',
  '/docs/architecture/structure',
];

function shouldUseDiagramViewport(pathname: string): boolean {
  return VIEWPORT_DOC_PATH_PREFIXES.some((prefix) =>
    pathname.startsWith(prefix),
  );
}

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

function MermaidRenderResult({
  renderResult,
  useViewport,
}: {
  renderResult: RenderResult;
  useViewport: boolean;
}): ReactNode {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const div = ref.current;
    if (div) {
      renderResult.bindFunctions?.(div);
    }
  }, [renderResult]);

  const diagram = (
    <div
      ref={ref}
      className={MermaidContainerClassName}
      // Mermaid returns a complete SVG string and optional click binders.
      // eslint-disable-next-line react/no-danger
      dangerouslySetInnerHTML={{__html: renderResult.svg}}
    />
  );

  if (!useViewport) {
    return diagram;
  }

  return <DiagramViewport>{diagram}</DiagramViewport>;
}

function MermaidRenderer({
  value,
  useViewport,
}: Props & {
  useViewport: boolean;
}): ReactNode {
  const defaultMermaidConfig = useMermaidConfig();
  const mermaidConfig = useMemo(() => {
    if (!useViewport) {
      return defaultMermaidConfig;
    }

    return {
      ...defaultMermaidConfig,
      flowchart: {
        ...defaultMermaidConfig.flowchart,
        useMaxWidth: false,
      },
    };
  }, [defaultMermaidConfig, useViewport]);

  const renderResult = useMermaidRenderResult({
    config: mermaidConfig,
    text: value,
  });
  if (renderResult === null) {
    return null;
  }

  return (
    <MermaidRenderResult
      renderResult={renderResult}
      useViewport={useViewport}
    />
  );
}

function MermaidWithIcons(props: Props): ReactNode {
  const {pathname} = useLocation();
  const [iconsReady, setIconsReady] = useState(false);
  const useViewport = shouldUseDiagramViewport(pathname);

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

  return <MermaidRenderer {...props} useViewport={useViewport} />;
}

export default function Mermaid(props: Props): ReactNode {
  return (
    <ErrorBoundary
      fallback={(params) => <ErrorBoundaryErrorMessageFallback {...params} />}>
      <MermaidWithIcons {...props} />
    </ErrorBoundary>
  );
}
