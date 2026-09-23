import {Composition} from 'remotion';
import {ComparisonScene} from './ComparisonScene';
import {impactSpec, measuredSavingsSpec} from './PixelComparison';

export const RemotionRoot = () => (
	<>
		<Composition
			id="PixelComparison"
			component={ComparisonScene}
			durationInFrames={360}
			fps={30}
			width={1600}
			height={1000}
			defaultProps={{spec: measuredSavingsSpec}}
		/>
		<Composition
			id="PixelImpact"
			component={ComparisonScene}
			durationInFrames={360}
			fps={30}
			width={1600}
			height={1000}
			defaultProps={{spec: impactSpec}}
		/>
	</>
);
