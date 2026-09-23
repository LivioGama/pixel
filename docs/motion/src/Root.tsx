import {Composition} from 'remotion';
import {ComparisonScene, ComparisonSpec} from './ComparisonScene';
import {impactSpec, measuredSavingsSpec, publishSpec, rollbackSpec, rewriteSpec, scopeSpec} from './PixelComparison';

const comps: [string, ComparisonSpec][] = [
	['PixelComparison', measuredSavingsSpec],
	['PixelImpact', impactSpec],
	['PixelScope', scopeSpec],
	['PixelRollback', rollbackSpec],
	['PixelPublish', publishSpec],
	['PixelRewrite', rewriteSpec],
];

export const RemotionRoot = () => (
	<>
		{comps.map(([id, spec]) => (
			<Composition
				key={id}
				id={id}
				component={ComparisonScene}
				durationInFrames={240}
				fps={30}
				width={1600}
				height={1000}
				defaultProps={{spec}}
			/>
		))}
	</>
);
