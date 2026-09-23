import {Composition} from 'remotion';
import {PixelComparison} from './PixelComparison';

export const RemotionRoot = () => (
	<Composition
		id="PixelComparison"
		component={PixelComparison}
		durationInFrames={360}
		fps={30}
		width={1600}
		height={1000}
	/>
);
