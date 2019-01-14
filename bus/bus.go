package bus

type Action struct{}

type Bus chan Action

func New() Bus {
	return make(Bus)
}
